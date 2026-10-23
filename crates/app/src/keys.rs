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
const SERVICE: &str = "ziggy";
/// Where the key was kept while the app was called Signal GPUI. Read when
/// [`SERVICE`] has none, and copied there; left in place, so the old app
/// can still read it.
#[cfg(target_os = "macos")]
const FORMER_SERVICE: &str = "signal-gpui";
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
/// `moved_in`: Signal GPUI's data just moved to `data_dir`, so its key is
/// the one that opens it.
pub fn load_or_create(data_dir: &Path, store_kind: KeyStore, moved_in: bool) -> Result<[u8; 32]> {
    let existing = match store_kind {
        KeyStore::System => load(data_dir, moved_in)?,
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
fn load(data_dir: &Path, moved_in: bool) -> Result<Option<[u8; 32]>> {
    // A `ziggy` key from before is for a profile of Ziggy's own that's gone
    // (the data moved in only because Ziggy's directory didn't exist), and
    // can't open Signal GPUI's databases.
    if !moved_in && let Some(key) = load_service(SERVICE)? {
        return Ok(Some(key));
    }
    // Never a new key for databases the old name's key encrypts.
    let Some(key) = load_service(FORMER_SERVICE)? else {
        return if moved_in { load_service(SERVICE) } else { Ok(None) };
    };
    store(data_dir, &key)?;
    tracing::info!("copied the database key from the Keychain item {FORMER_SERVICE:?} to {SERVICE:?}");
    Ok(Some(key))
}

#[cfg(target_os = "macos")]
fn load_service(service: &str) -> Result<Option<[u8; 32]>> {
    use security_framework::passwords::get_generic_password;
    match get_generic_password(service, ACCOUNT) {
        Ok(bytes) => decode(&bytes).map(Some),
        // errSecItemNotFound
        Err(e) if e.code() == -25300 => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading database key {service:?} from the Keychain")),
    }
}

#[cfg(target_os = "macos")]
fn store(_data_dir: &Path, key: &[u8; 32]) -> Result<()> {
    security_framework::passwords::set_generic_password(SERVICE, ACCOUNT, hex::encode(key).as_bytes())
        .context("storing database key in the Keychain")
}

#[cfg(not(target_os = "macos"))]
fn load(data_dir: &Path, _moved_in: bool) -> Result<Option<[u8; 32]>> {
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
