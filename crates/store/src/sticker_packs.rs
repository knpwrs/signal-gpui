//! Sticker packs we know of, as Signal Desktop keeps them
//! (`ts/types/Stickers.preload.ts`, `ts/sql/Server.node.ts`'s sticker
//! pack functions): a pack named in a sticker we received is known; its
//! title, author and cover come once fetched; installed ones have a place
//! in the order, and an uninstall is remembered for Storage Service. The
//! installed packs' stickers themselves are presage's.

use sqlx::Row as _;

use crate::{AppDb, Result};

/// A pack, by its hex id and key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StickerPackRow {
    pub id: String,
    pub key: String,
    pub title: Option<String>,
    pub author: Option<String>,
    pub cover_path: Option<String>,
    pub sticker_count: u32,
    pub installed: bool,
    pub position: Option<u32>,
    pub uninstalled_at: Option<i64>,
    pub created_at: i64,
}

impl AppDb {
    /// Every pack we know of, newest first (Desktop's `createdAt` order).
    pub async fn sticker_pack_rows(&self) -> Result<Vec<StickerPackRow>> {
        let rows = sqlx::query(
            "SELECT id, key, title, author, cover_path, sticker_count, installed, position, uninstalled_at, created_at \
             FROM sticker_packs ORDER BY created_at DESC, id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(StickerPackRow {
                    id: row.try_get("id")?,
                    key: row.try_get("key")?,
                    title: row.try_get("title")?,
                    author: row.try_get("author")?,
                    cover_path: row.try_get("cover_path")?,
                    sticker_count: row.try_get::<i64, _>("sticker_count")? as u32,
                    installed: row.try_get("installed")?,
                    position: row.try_get::<Option<i64>, _>("position")?.map(|p| p as u32),
                    uninstalled_at: row.try_get("uninstalled_at")?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect()
    }

    /// A pack named in a sticker (Desktop's `savePackMetadata`, status
    /// "known"): kept if it's new, with its key. A pack we know keeps the
    /// key it has. `true` when it was new.
    pub async fn sticker_pack_seen(&self, id: &str, key: &str, now: i64) -> Result<bool> {
        let added = sqlx::query("INSERT INTO sticker_packs (id, key, created_at) VALUES (?, ?, ?) ON CONFLICT (id) DO NOTHING")
            .bind(id)
            .bind(key)
            .bind(now)
            .execute(&self.writer)
            .await?
            .rows_affected();
        Ok(added > 0)
    }

    /// What a pack's manifest says, and its downloaded cover.
    pub async fn set_sticker_pack_meta(&self, id: &str, title: &str, author: &str, cover_path: Option<&str>, count: u32) -> Result<()> {
        sqlx::query(
            "UPDATE sticker_packs SET title = ?, author = ?, cover_path = COALESCE(?, cover_path), sticker_count = ? WHERE id = ?",
        )
        .bind(title)
        .bind(author)
        .bind(cover_path)
        .bind(i64::from(count))
        .bind(id)
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Marks a pack installed (Desktop's `installStickerPack`): at
    /// `position`, or after every installed one. Returns its position and
    /// whether it wasn't installed before.
    pub async fn install_sticker_pack_row(&self, id: &str, key: &str, position: Option<u32>, now: i64) -> Result<(u32, bool)> {
        let mut tx = self.write().await?;
        let before: Option<bool> =
            sqlx::query_scalar("SELECT installed FROM sticker_packs WHERE id = ?").bind(id).fetch_optional(&mut *tx).await?;
        let position = match position {
            Some(position) => position,
            None => {
                let max: Option<i64> =
                    sqlx::query_scalar("SELECT MAX(position) FROM sticker_packs WHERE installed = 1").fetch_one(&mut *tx).await?;
                max.map_or(0, |m| m as u32 + 1)
            }
        };
        sqlx::query(
            "INSERT INTO sticker_packs (id, key, installed, position, created_at) VALUES (?1, ?2, 1, ?3, ?4) \
             ON CONFLICT (id) DO UPDATE SET key = ?2, installed = 1, position = ?3, uninstalled_at = NULL",
        )
        .bind(id)
        .bind(key)
        .bind(i64::from(position))
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((position, before != Some(true)))
    }

    /// Marks a pack uninstalled at `at` (Desktop's `uninstallStickerPack`);
    /// `true` when it was installed.
    pub async fn uninstall_sticker_pack_row(&self, id: &str, at: i64) -> Result<bool> {
        let changed = sqlx::query(
            "UPDATE sticker_packs SET installed = 0, position = NULL, uninstalled_at = ? WHERE id = ? AND installed = 1",
        )
        .bind(at)
        .bind(id)
        .execute(&self.writer)
        .await?
        .rows_affected();
        Ok(changed > 0)
    }

    /// Installed packs in this order (Desktop's `updateStickerPacksPositions`).
    pub async fn set_sticker_pack_order(&self, ids: &[String]) -> Result<()> {
        let mut tx = self.write().await?;
        for (position, id) in ids.iter().enumerate() {
            sqlx::query("UPDATE sticker_packs SET position = ? WHERE id = ? AND installed = 1")
                .bind(position as i64)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
