//! Debug logs, following Signal Desktop (`ts/logging/debuglogs.node.ts`,
//! `ts/util/privacy.node.ts`, `ts/logging/uploadDebugLog.node.ts`):
//! system and account info, then the log files, with phone numbers,
//! account and group ids, keys and our own paths redacted. It can be saved
//! or uploaded to debuglogs.org, which returns a link to share.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context as _, bail};
use regex::Regex;

const REDACTED: &str = "[REDACTED]";
const UPLOAD_URL: &str = "https://debuglogs.org";
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// Log files live here, one per day (see the app's logging setup).
pub fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

static PHONE_NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\+\d{7,12}(\d{3})").unwrap());
static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[0-9A-F]{8}-[0-9A-F]{4}-[0-8][0-9A-F]{3}-[089AB][0-9A-F]{3}-[0-9A-F]{9}([0-9A-F]{3})").unwrap()
});
static GROUP_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(group\()([^)]+)(\))").unwrap());
static GROUP_V2_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(groupv2\()([^=)]+)(=?=?\))").unwrap());
static CALL_LINK_ROOM_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)[0-9A-F]{61}([0-9A-F]{3})").unwrap());
static CALL_LINK_ROOT_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)([A-Z]{4})-[A-Z]{4}-[A-Z]{4}-[A-Z]{4}-[A-Z]{4}-[A-Z]{4}-[A-Z]{4}-[A-Z]{4}").unwrap()
});
static ATTACHMENT_URL_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(attachment://\S+key=)(\S+)").unwrap());
static CARD_NUMBER_OR_STACK_ADDRESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"0x[0-9a-f]{16}|\d\d(\d-?){11,16}\d").unwrap());
// Our own: group conversation ids are 64 hex digits (the master key).
static GROUP_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bg:[0-9a-f]{61}([0-9a-f]{3})\b").unwrap());

/// Desktop's `redactAll`, plus our hex group keys. Keeps the last three
/// characters of ids so lines can still be told apart.
pub fn redact(text: &str, sensitive_paths: &[PathBuf]) -> String {
    let text = ATTACHMENT_URL_KEY.replace_all(text, format!("${{1}}{REDACTED}"));
    let text = CALL_LINK_ROOM_ID.replace_all(&text, format!("{REDACTED}${{1}}"));
    let text = CALL_LINK_ROOT_KEY.replace_all(&text, format!("{REDACTED}${{1}}"));
    let text = GROUP_KEY.replace_all(&text, format!("g:{REDACTED}${{1}}"));
    let last3 = |caps: &regex::Captures| {
        let id: String = caps[2].chars().filter(|c| *c != '\n' && *c != '\r').collect();
        let tail = &id[id.char_indices().rev().nth(2).map_or(0, |(i, _)| i)..];
        format!("{}{REDACTED}{tail}{}", &caps[1], &caps[3])
    };
    let text = GROUP_ID.replace_all(&text, last3);
    let text = GROUP_V2_ID.replace_all(&text, last3);
    let text = PHONE_NUMBER.replace_all(&text, format!("+{REDACTED}${{1}}"));
    let text = UUID.replace_all(&text, format!("{REDACTED}${{1}}"));
    // These leave nothing.
    let mut text = CARD_NUMBER_OR_STACK_ADDRESS
        .replace_all(&text, |caps: &regex::Captures| {
            let found = &caps[0];
            if found.starts_with("0x") { found.to_owned() } else { REDACTED.to_owned() }
        })
        .into_owned();
    // Longest first, so a path inside another is still caught whole.
    let mut paths: Vec<String> =
        sensitive_paths.iter().map(|p| p.to_string_lossy().into_owned()).filter(|p| p.len() > 1).collect();
    paths.sort_by_key(|p| std::cmp::Reverse(p.len()));
    for path in paths {
        text = text.replace(&format!("file://{path}"), REDACTED).replace(&path, REDACTED);
    }
    text
}

/// What goes above the log lines.
#[derive(Debug, Clone, Default)]
pub struct Header {
    pub app_version: String,
    pub os_version: String,
    pub arch: String,
    /// `(label, value)`, e.g. the account's phone number and id.
    pub user: Vec<(String, String)>,
}

fn section(title: &str, rows: &[(String, String)], sensitive_paths: &[PathBuf]) -> String {
    let mut rows = rows.to_vec();
    rows.sort();
    let mut out = format!("========= {title} =========\n");
    for (key, value) in rows {
        out.push_str(&format!("{key}: {}\n", redact(&value, sensitive_paths)));
    }
    out.push('\n');
    out
}

/// The whole debug log: header sections, then every log file oldest first,
/// redacted.
pub fn assemble(log_dir: &Path, header: &Header, sensitive_paths: &[PathBuf]) -> String {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let system = [
        ("Time".to_owned(), now),
        ("App version".to_owned(), header.app_version.clone()),
        ("OS version".to_owned(), header.os_version.clone()),
        ("Arch".to_owned(), header.arch.clone()),
    ];
    let mut out = section("System info", &system, sensitive_paths);
    out.push_str(&section("User info", &header.user, sensitive_paths));
    out.push_str("========= Logs =========\n");

    let mut files: Vec<PathBuf> = std::fs::read_dir(log_dir)
        .map(|entries| entries.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect())
        .unwrap_or_default();
    // Daily files are named by date, so name order is time order.
    files.sort();
    for file in files {
        match std::fs::read(&file) {
            Ok(bytes) => out.push_str(&redact(&String::from_utf8_lossy(&bytes), sensitive_paths)),
            Err(error) => out.push_str(&format!("(couldn't read a log file: {error})\n")),
        }
    }
    out
}

/// Uploads a debug log to debuglogs.org (gzipped) and returns its link.
pub async fn upload(log: &str, app_version: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(crate::link_preview::tls_config()?)
        .user_agent(format!("ziggy/{app_version}"))
        .timeout(UPLOAD_TIMEOUT)
        .build()?;

    // A signed S3 form: where to post, and the fields to post with it.
    let form: serde_json::Value = client
        .get(UPLOAD_URL)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("reading upload form")?;
    let target = form["url"].as_str().context("upload form has no url")?;
    if !target.starts_with("https://") {
        bail!("upload form url isn't https");
    }
    let fields = form["fields"].as_object().context("upload form has no fields")?;
    let key = format!("{}.gz", fields.get("key").and_then(|k| k.as_str()).context("upload form has no key")?);

    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(log.as_bytes())?;
    let body = gz.finish()?;

    // The key has to come first.
    let mut multipart = reqwest::multipart::Form::new().text("key", key.clone());
    for (name, value) in fields.iter().filter(|(name, _)| *name != "key") {
        let value = value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string());
        multipart = multipart.text(name.clone(), value);
    }
    multipart = multipart.text("Content-Type", "application/gzip").part(
        "file",
        reqwest::multipart::Part::bytes(body)
            .file_name(format!("ziggy-debug-log-{app_version}.txt.gz"))
            .mime_str("application/gzip")?,
    );
    let response = client.post(target).multipart(multipart).send().await?;
    if response.status().as_u16() != 204 {
        bail!("upload failed: {}", response.status());
    }
    Ok(format!("{UPLOAD_URL}/{key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_like_desktop() {
        let paths = [PathBuf::from("/Users/me/Library/Application Support/io.github.knpwrs.Ziggy")];
        let line = "send to +15551234567 aci 6b1a2b3c-0000-4000-8000-000000000abc in groupv2(abcdefXYZ==) \
                    file /Users/me/Library/Application Support/io.github.knpwrs.Ziggy/attachments/1.png \
                    card 4111-1111-1111-1111 at 0x00007ff8a1b2c3d4 \
                    conversation g:0000000000000000000000000000000000000000000000000000000000000123";
        let redacted = redact(line, &paths);
        assert_eq!(
            redacted,
            "send to +[REDACTED]567 aci [REDACTED]abc in groupv2([REDACTED]XYZ==) \
             file [REDACTED]/attachments/1.png \
             card [REDACTED] at 0x00007ff8a1b2c3d4 \
             conversation g:[REDACTED]123"
        );
    }

    #[test]
    fn assembles_header_then_logs_in_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ziggy.2026-10-13.log"), "older +15551234567\n").unwrap();
        std::fs::write(dir.path().join("ziggy.2026-10-14.log"), "newer\n").unwrap();
        let header = Header {
            app_version: "0.1.0".into(),
            os_version: "macOS 26".into(),
            arch: "aarch64".into(),
            user: vec![("Phone".into(), "+15557654321".into())],
        };
        let log = assemble(dir.path(), &header, &[]);
        assert!(log.starts_with("========= System info ========="), "{log}");
        assert!(log.contains("Phone: +[REDACTED]321"));
        let logs = &log[log.find("========= Logs =========").unwrap()..];
        assert!(logs.find("older +[REDACTED]567").unwrap() < logs.find("newer").unwrap());
    }
}
