//! Database key management.
//!
//! Signal Desktop generates a random SQLCipher key and protects it with the
//! OS credential store (Electron `safeStorage`). We do the same: a random
//! 256-bit key lives in the macOS Keychain. Elsewhere it falls back to a
//! `0600` key file next to the databases.

use std::path::Path;

use anyhow::{Context as _, Result};
use rand::RngCore as _;

#[cfg(target_os = "macos")]
const SERVICE: &str = "signal-gpui";
#[cfg(target_os = "macos")]
const ACCOUNT: &str = "database-key";

/// Where the key lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStore {
    /// The OS credential store (macOS Keychain); file fallback elsewhere.
    System,
    /// A `0600` file in the data directory. Used for development profiles
    /// (`SIGNAL_GPUI_DATA_DIR`) so they don't touch the login Keychain.
    File,
}

/// Loads the database key, creating and storing a new one on first run.
pub fn load_or_create(data_dir: &Path, store_kind: KeyStore) -> Result<[u8; 32]> {
    let existing = match store_kind {
        KeyStore::System => load(data_dir)?,
        KeyStore::File => load_file(data_dir)?,
    };
    if let Some(key) = existing {
        return Ok(key);
    }
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    match store_kind {
        KeyStore::System => store(data_dir, &key)?,
        KeyStore::File => store_file(data_dir, &key)?,
    }
    Ok(key)
}

fn decode(hex_key: &[u8]) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_key.trim_ascii()).context("stored database key is not hex")?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("stored database key has the wrong length"))
}

#[cfg(target_os = "macos")]
fn load(_data_dir: &Path) -> Result<Option<[u8; 32]>> {
    use security_framework::passwords::get_generic_password;
    match get_generic_password(SERVICE, ACCOUNT) {
        Ok(bytes) => decode(&bytes).map(Some),
        // errSecItemNotFound
        Err(e) if e.code() == -25300 => Ok(None),
        Err(e) => Err(e).context("reading database key from the Keychain"),
    }
}

#[cfg(target_os = "macos")]
fn store(_data_dir: &Path, key: &[u8; 32]) -> Result<()> {
    security_framework::passwords::set_generic_password(SERVICE, ACCOUNT, hex::encode(key).as_bytes())
        .context("storing database key in the Keychain")
}

#[cfg(not(target_os = "macos"))]
fn load(data_dir: &Path) -> Result<Option<[u8; 32]>> {
    load_file(data_dir)
}

#[cfg(not(target_os = "macos"))]
fn store(data_dir: &Path, key: &[u8; 32]) -> Result<()> {
    store_file(data_dir, key)
}

fn load_file(data_dir: &Path) -> Result<Option<[u8; 32]>> {
    match std::fs::read(data_dir.join("db.key")) {
        Ok(bytes) => decode(&bytes).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("reading database key"),
    }
}

fn store_file(data_dir: &Path, key: &[u8; 32]) -> Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(data_dir.join("db.key")).context("creating database key file")?;
    file.write_all(hex::encode(key).as_bytes())?;
    Ok(())
}
