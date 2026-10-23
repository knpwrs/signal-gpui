//! Stored file paths are absolute: attachments, thumbnails, previews,
//! stories, avatars, badges and sticker covers name files in the data
//! directory. When the directory moves (Signal GPUI's became Ziggy's),
//! they're rewritten to name the new one.

use sqlx::Row as _;

use crate::{AppDb, Result};

/// The data directory whose paths were last rewritten.
pub const SETTING_REBASED_FROM: &str = "data_dir.rebased_from";

/// Columns holding what people wrote (message text and its previews,
/// drafts, names, notes), which may quote a path but never store one.
const WRITTEN: &[&str] = &[
    "about",
    "about_emoji",
    "author_name",
    "body",
    "description",
    "draft",
    "family",
    "family_name",
    "given",
    "given_name",
    "last_message_preview",
    "name",
    "note",
    "plain_name",
    "title",
    "username",
];

impl AppDb {
    /// Rewrites every stored path to a file in `from` (a former data
    /// directory) to name it in `to`, in every column of every table but
    /// those of [`WRITTEN`], so that a path kept somewhere new (a JSON
    /// field, a setting) isn't missed. Only a path is rewritten: a value
    /// that starts with `from`, or a JSON string that does (just after its
    /// `"`), never `from` in the middle of a sentence. Full-text search
    /// tables are left alone. One transaction per table. Returns how many
    /// rows changed.
    pub async fn rebase_paths(&self, from: &str, to: &str) -> Result<u64> {
        let (from, to) = (format!("{}/", from.trim_end_matches('/')), format!("{}/", to.trim_end_matches('/')));
        let tables: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, sql FROM sqlite_schema WHERE type = 'table' \
             AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' AND name NOT LIKE '\\_sqlx%' ESCAPE '\\'",
        )
        .fetch_all(&self.pool)
        .await?;
        // A virtual table's own tables are named after it (`messages_fts_data`).
        let virtual_tables: Vec<&str> = tables
            .iter()
            .filter(|(_, sql)| sql.trim_start().to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE"))
            .map(|(name, _)| name.as_str())
            .collect();
        let mut changed = 0;
        for (table, _) in &tables {
            if virtual_tables.iter().any(|v| table == v || table.starts_with(&format!("{v}_"))) {
                continue;
            }
            let columns: Vec<String> = sqlx::query("SELECT name FROM pragma_table_info(?)")
                .bind(table)
                .fetch_all(&self.pool)
                .await?
                .iter()
                .map(|row| row.try_get::<String, _>("name"))
                .collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|column| !WRITTEN.contains(&column.as_str()))
                .collect();
            if columns.is_empty() {
                continue;
            }
            let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
            // ?1 and ?2: the directories; ?3 and ?4: as a JSON string
            // starts. Only text: `replace` would turn a number or a blob
            // into text.
            let sets = columns
                .iter()
                .map(|c| {
                    format!(
                        "{0} = CASE WHEN typeof({0}) = 'text' THEN replace(\
                         CASE WHEN substr({0}, 1, length(?1)) = ?1 THEN ?2 || substr({0}, length(?1) + 1) ELSE {0} END, \
                         ?3, ?4) ELSE {0} END",
                        quote(c)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let any = columns
                .iter()
                .map(|c| format!("(typeof({0}) = 'text' AND (substr({0}, 1, length(?1)) = ?1 OR instr({0}, ?3) > 0))", quote(c)))
                .collect::<Vec<_>>()
                .join(" OR ");
            let mut tx = self.write().await?;
            let done = sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE {} SET {sets} WHERE {any}", quote(table))))
                .bind(&from)
                .bind(&to)
                .bind(format!("\"{from}"))
                .bind(format!("\"{to}"))
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            changed += done.rows_affected();
        }
        Ok(changed)
    }
}
