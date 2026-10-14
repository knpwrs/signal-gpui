//! Emoji recents and frequency, used for the picker's "Recents" row and to
//! boost type-ahead ranking.

use std::collections::HashMap;

use signal_emoji::EmojiId;
use signal_store::EmojiUsage;

/// Number of recents shown in the picker (Signal shows two rows' worth).
pub const MAX_RECENTS: usize = 32;

#[derive(Default, Clone)]
pub struct EmojiUsageCache {
    /// Base emoji id -> (count, last used millis).
    by_id: HashMap<EmojiId, (u32, i64)>,
    /// Glyphs most-recent first, as picked (with skin tone).
    recents: Vec<String>,
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl EmojiUsageCache {
    pub fn from_usage(mut rows: Vec<EmojiUsage>) -> Self {
        rows.sort_by_key(|row| std::cmp::Reverse(row.last_used));
        let mut cache = Self::default();
        for row in rows {
            let Some((emoji, _)) = signal_emoji::by_glyph(&row.emoji) else {
                continue;
            };
            let entry = cache.by_id.entry(emoji.id).or_insert((0, row.last_used));
            entry.0 += row.count;
            entry.1 = entry.1.max(row.last_used);
            if cache.recents.len() < MAX_RECENTS && !cache.recents.contains(&row.emoji) {
                cache.recents.push(row.emoji);
            }
        }
        cache
    }

    pub fn record(&mut self, glyph: &str) {
        if let Some((emoji, _)) = signal_emoji::by_glyph(glyph) {
            let entry = self.by_id.entry(emoji.id).or_insert((0, 0));
            entry.0 += 1;
            entry.1 = now_millis();
        }
        self.recents.retain(|g| g != glyph);
        self.recents.insert(0, glyph.to_owned());
        self.recents.truncate(MAX_RECENTS);
    }

    pub fn recents(&self) -> &[String] {
        &self.recents
    }

    /// Ranking boost: frequent and recent picks float up, but never beat a
    /// much better textual match (max boost ≈ 35 vs. 40 for an exact term).
    pub fn boost(&self, id: EmojiId) -> f32 {
        let Some(&(count, last_used)) = self.by_id.get(&id) else {
            return 0.0;
        };
        let frequency = (1.0 + count as f32).ln() * 6.0;
        let days = ((now_millis() - last_used).max(0) as f32) / 86_400_000.0;
        let recency = 15.0 / (1.0 + days);
        (frequency + recency).min(35.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_boosts() {
        let mut cache = EmojiUsageCache::default();
        let pizza = signal_emoji::by_shortcode("pizza").unwrap();
        assert_eq!(cache.boost(pizza.id), 0.0);
        cache.record("🍕");
        cache.record("👍🏽");
        assert!(cache.boost(pizza.id) > 0.0);
        assert_eq!(cache.recents(), ["👍🏽", "🍕"]);
        // Skin-toned picks boost the base emoji.
        let thumbs = signal_emoji::by_shortcode("+1").unwrap();
        assert!(cache.boost(thumbs.id) > 0.0);
    }

    #[test]
    fn loads_rows_most_recent_first() {
        let cache = EmojiUsageCache::from_usage(vec![
            EmojiUsage { emoji: "🍕".into(), count: 3, last_used: 1 },
            EmojiUsage { emoji: "🎉".into(), count: 1, last_used: 2 },
        ]);
        assert_eq!(cache.recents(), ["🎉", "🍕"]);
    }
}
