//! People's profiles as last fetched (name, about, badges) and their
//! usernames from our contact records, as Signal Desktop keeps them on a
//! conversation; and badges, as Desktop's `badges` and `badgeImageFiles`
//! tables keep them (`ts/badges/`).

use std::collections::HashMap;

use crate::{AppDb, Result, combine_names};

/// A badge on someone's profile.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProfileBadge {
    pub id: String,
    /// Only on our own profile: when it expires (unix millis) and whether
    /// we show it (Desktop's `expiresAt` and `isVisible`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
}

impl ProfileBadge {
    /// Desktop's `isBadgeVisible`: others' badges are all shown; ours as
    /// we chose.
    pub fn is_visible(&self) -> bool {
        self.visible.unwrap_or(true)
    }
}

/// Someone's profile and username.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersonProfile {
    pub service_id: String,
    /// Their profile name.
    pub given_name: String,
    pub family_name: String,
    pub about: String,
    pub about_emoji: String,
    pub badges: Vec<ProfileBadge>,
    /// When the profile was fetched (unix millis); 0 if never.
    pub fetched_at: i64,
    /// Their username, from our contact record.
    pub username: Option<String>,
}

impl PersonProfile {
    /// Their profile name, given and family name combined.
    pub fn profile_name(&self) -> Option<String> {
        combine_names(&self.given_name, &self.family_name)
    }

    /// The badges shown on their avatar, the first one first (Desktop shows
    /// `badges[0]`, the "preferred badge").
    pub fn visible_badges(&self) -> impl Iterator<Item = &ProfileBadge> {
        self.badges.iter().filter(|b| b.is_visible())
    }
}

/// What a profile fetch tells us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchedProfile {
    pub given_name: String,
    pub family_name: String,
    pub about: String,
    pub about_emoji: String,
    pub badges: Vec<ProfileBadge>,
}

/// A badge's kind (Desktop's `BadgeCategory`; anything else, "testing"
/// too, is `Other`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeCategory {
    Donor,
    Other,
}

impl BadgeCategory {
    pub fn parse(value: &str) -> Self {
        if value == "donor" { Self::Donor } else { Self::Other }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Donor => "donor",
            Self::Other => "other",
        }
    }
}

/// One size of a badge's image, in its themes (Desktop's
/// `BadgeImageType`): URLs.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BadgeImage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dark: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transparent: Option<String>,
}

/// Which image of a badge to draw (Desktop's `BadgeImageTheme`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeTheme {
    Light,
    Dark,
    Transparent,
}

/// A badge as the server describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    pub id: String,
    pub category: BadgeCategory,
    pub name: String,
    /// With `{short_name}` for the holder's first name.
    pub description: String,
    /// Four sizes, smallest first: three themed, then one transparent
    /// (Desktop's `parseImages`).
    pub images: Vec<BadgeImage>,
}

impl Badge {
    /// Every image URL, to download.
    pub fn urls(&self) -> impl Iterator<Item = &str> {
        self.images.iter().flat_map(|i| [&i.light, &i.dark, &i.transparent]).flatten().map(String::as_str)
    }

    /// The image URL to draw at `size` pixels in `theme`, as Desktop's
    /// `getBadgeImageFileLocalPath` picks it among the downloaded ones
    /// (`have` says which are).
    pub fn image_for(&self, size: f32, theme: BadgeTheme, have: impl Fn(&str) -> bool) -> Option<String> {
        let pick = |image: &BadgeImage| -> Option<String> {
            let url = match theme {
                BadgeTheme::Light => image.light.as_ref(),
                BadgeTheme::Dark => image.dark.as_ref(),
                BadgeTheme::Transparent => image.transparent.as_ref(),
            }?;
            have(url).then(|| url.clone())
        };
        let paths: Vec<Option<String>> = self.images.iter().map(pick).collect();
        if theme == BadgeTheme::Transparent {
            return if size < 36. {
                paths.iter().flatten().next().cloned()
            } else {
                paths.iter().rev().flatten().next().cloned()
            };
        }
        let at = |ix: usize| paths.get(ix).cloned().flatten();
        if size < 24. {
            at(0)
        } else if size < 36. {
            at(1)
        } else if size < 160. {
            at(2)
        } else {
            paths.last().cloned().flatten().or_else(|| at(2))
        }
    }

    /// The description with the holder's first name in it (Desktop's
    /// `BadgeDescription`).
    pub fn describe(&self, first_name: &str) -> String {
        self.description.replace("{short_name}", first_name)
    }
}

/// A `profiles` row: given and family name, about, about emoji, badges
/// (JSON), fetch time and username.
type ProfileRow = (String, String, String, String, String, i64, Option<String>);

impl AppDb {
    /// Someone's profile, `None` when nothing is known.
    pub async fn person_profile(&self, service_id: &str) -> Result<Option<PersonProfile>> {
        let row: Option<ProfileRow> = sqlx::query_as(
            "SELECT given_name, family_name, about, about_emoji, badges, fetched_at, username \
             FROM profiles WHERE service_id = ?",
        )
        .bind(service_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((given_name, family_name, about, about_emoji, badges, fetched_at, username)) = row else {
            return Ok(None);
        };
        Ok(Some(PersonProfile {
            service_id: service_id.to_owned(),
            given_name,
            family_name,
            about,
            about_emoji,
            badges: serde_json::from_str(&badges).unwrap_or_default(),
            fetched_at,
            username,
        }))
    }

    /// Saves what a profile fetch at `fetched_at` returned. Returns whether
    /// anything shown changed (not just the time).
    pub async fn save_fetched_profile(&self, service_id: &str, profile: &FetchedProfile, fetched_at: i64) -> Result<bool> {
        Ok(self.save_fetched_profile_noting_name(service_id, profile, fetched_at).await?.0)
    }

    /// [`AppDb::save_fetched_profile`], also returning their profile name
    /// before and after (given and family name combined) when it changed
    /// from one they had, as Desktop's `setEncryptedProfileName` notices
    /// (read and written in one transaction, so two fetches at once can't
    /// both see the old name).
    pub async fn save_fetched_profile_noting_name(
        &self,
        service_id: &str,
        profile: &FetchedProfile,
        fetched_at: i64,
    ) -> Result<(bool, Option<(String, String)>)> {
        let badges = serde_json::to_string(&profile.badges)?;
        let mut tx = self.write().await?;
        let old: Option<(String, String, String, String, String)> = sqlx::query_as(
            "SELECT given_name, family_name, about, about_emoji, badges FROM profiles WHERE service_id = ?",
        )
        .bind(service_id)
        .fetch_optional(&mut *tx)
        .await?;
        let new = (
            profile.given_name.clone(),
            profile.family_name.clone(),
            profile.about.clone(),
            profile.about_emoji.clone(),
            badges.clone(),
        );
        sqlx::query(
            "INSERT INTO profiles (service_id, given_name, family_name, about, about_emoji, badges, fetched_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT (service_id) DO UPDATE SET given_name = ?2, family_name = ?3, about = ?4, \
                about_emoji = ?5, badges = ?6, fetched_at = ?7",
        )
        .bind(service_id)
        .bind(&profile.given_name)
        .bind(&profile.family_name)
        .bind(&profile.about)
        .bind(&profile.about_emoji)
        .bind(&badges)
        .bind(fetched_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        let old_name = old.as_ref().and_then(|(given, family, ..)| combine_names(given, family));
        let new_name = combine_names(&profile.given_name, &profile.family_name);
        let name_change = match old_name {
            Some(old_name) if Some(&old_name) != new_name.as_ref() => Some((old_name, new_name.unwrap_or_default())),
            _ => None,
        };
        Ok((old.as_ref() != Some(&new), name_change))
    }

    /// Sets our own profile name (from the account record, or after we
    /// wrote our profile) without touching the rest. Returns whether it
    /// changed.
    pub async fn set_profile_name(&self, service_id: &str, given: &str, family: &str) -> Result<bool> {
        let result = sqlx::query(
            "INSERT INTO profiles (service_id, given_name, family_name) VALUES (?1, ?2, ?3) \
             ON CONFLICT (service_id) DO UPDATE SET given_name = ?2, family_name = ?3 \
             WHERE given_name IS NOT ?2 OR family_name IS NOT ?3",
        )
        .bind(service_id)
        .bind(given)
        .bind(family)
        .execute(&self.writer)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Replaces everyone's username with what the contact records say:
    /// `(service id, username)`. People not listed have none.
    pub async fn replace_usernames(&self, usernames: &[(String, String)]) -> Result<()> {
        let mut tx = self.write().await?;
        sqlx::query("UPDATE profiles SET username = NULL WHERE username IS NOT NULL").execute(&mut *tx).await?;
        for (service_id, username) in usernames {
            sqlx::query(
                "INSERT INTO profiles (service_id, username) VALUES (?1, ?2) \
                 ON CONFLICT (service_id) DO UPDATE SET username = ?2",
            )
            .bind(service_id)
            .bind(username)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Everyone whose profile shows a badge: their visible badges' ids,
    /// first one first.
    pub async fn badge_holders(&self) -> Result<HashMap<String, Vec<String>>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT service_id, badges FROM profiles WHERE badges != '[]'").fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .filter_map(|(sid, badges)| {
                let badges: Vec<ProfileBadge> = serde_json::from_str(&badges).ok()?;
                let ids: Vec<String> = badges.into_iter().filter(|b| b.is_visible()).map(|b| b.id).collect();
                (!ids.is_empty()).then_some((sid, ids))
            })
            .collect())
    }

    /// Saves badge descriptions (replacing ones with the same id).
    pub async fn save_badges(&self, badges: &[Badge]) -> Result<()> {
        if badges.is_empty() {
            return Ok(());
        }
        let mut tx = self.write().await?;
        for badge in badges {
            sqlx::query(
                "INSERT INTO badges (id, category, name, description, images) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT (id) DO UPDATE SET category = ?2, name = ?3, description = ?4, images = ?5",
            )
            .bind(&badge.id)
            .bind(badge.category.as_str())
            .bind(&badge.name)
            .bind(&badge.description)
            .bind(serde_json::to_string(&badge.images)?)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Every badge we know of.
    pub async fn badges(&self) -> Result<Vec<Badge>> {
        let rows: Vec<(String, String, String, String, String)> =
            sqlx::query_as("SELECT id, category, name, description, images FROM badges").fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|(id, category, name, description, images)| Badge {
                id,
                category: BadgeCategory::parse(&category),
                name,
                description,
                images: serde_json::from_str(&images).unwrap_or_default(),
            })
            .collect())
    }

    /// Downloaded badge images: URL to local path.
    pub async fn badge_image_files(&self) -> Result<HashMap<String, String>> {
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT url, path FROM badge_image_files").fetch_all(&self.pool).await?;
        Ok(rows.into_iter().collect())
    }

    pub async fn set_badge_image_file(&self, url: &str, path: &str) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO badge_image_files (url, path) VALUES (?, ?)")
            .bind(url)
            .bind(path)
            .execute(&self.writer)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn badge() -> Badge {
        let image = |n: u8| BadgeImage {
            light: Some(format!("l{n}")),
            dark: Some(format!("d{n}")),
            transparent: None,
        };
        Badge {
            id: "R_LOW".into(),
            category: BadgeCategory::Donor,
            name: "Signal Sustainer".into(),
            description: "{short_name} supports Signal".into(),
            images: vec![image(1), image(2), image(3), BadgeImage { transparent: Some("t".into()), ..Default::default() }],
        }
    }

    #[test]
    fn badge_images_follow_desktops_sizes() {
        let badge = badge();
        let all = |_: &str| true;
        assert_eq!(badge.image_for(16., BadgeTheme::Light, all).as_deref(), Some("l1"));
        assert_eq!(badge.image_for(24., BadgeTheme::Dark, all).as_deref(), Some("d2"));
        assert_eq!(badge.image_for(36., BadgeTheme::Light, all).as_deref(), Some("l3"));
        // The largest is the transparent one, which has no light image:
        // Desktop falls back to the third.
        assert_eq!(badge.image_for(200., BadgeTheme::Light, all).as_deref(), Some("l3"));
        assert_eq!(badge.image_for(16., BadgeTheme::Transparent, all).as_deref(), Some("t"));
        // Only downloaded ones count.
        assert_eq!(badge.image_for(24., BadgeTheme::Light, |u| u != "l2"), None);
        assert_eq!(badge.describe("Maya"), "Maya supports Signal");
    }

    #[test]
    fn our_hidden_badges_stay_off_the_avatar() {
        let profile = PersonProfile {
            badges: vec![
                ProfileBadge { id: "a".into(), expires_at: Some(1), visible: Some(false) },
                ProfileBadge { id: "b".into(), expires_at: None, visible: None },
            ],
            ..Default::default()
        };
        assert_eq!(profile.visible_badges().map(|b| b.id.as_str()).collect::<Vec<_>>(), ["b"]);
    }
}
