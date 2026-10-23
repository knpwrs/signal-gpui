//! Starting with an empty data directory must ask for linking without touching the network.

use futures::{StreamExt, executor::block_on};
use signal_backend::{Backend, Config, Event};

#[test]
fn fresh_install_needs_linking() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_owned(),
        db_key: [3u8; 32],
        device_name: "test".into(),
        demo: false,
        former_data_dir: None,
    };
    let (backend, mut events) = Backend::start(config).unwrap();
    assert_eq!(block_on(events.next()), Some(Event::NeedsLinking));

    // AppDb queries run on the backend runtime and can be awaited from any executor.
    let db = backend.db().clone();
    let conversations = block_on(backend.spawn(async move { db.conversations().await })).unwrap();
    assert!(conversations.is_empty());

    assert!(dir.path().join("signal.db").exists());
    assert!(dir.path().join("app.db").exists());
    assert!(dir.path().join("attachments").is_dir());
    // Both databases are SQLCipher-encrypted (no plaintext SQLite header).
    for file in ["signal.db", "app.db"] {
        let bytes = std::fs::read(dir.path().join(file)).unwrap();
        assert!(!bytes.starts_with(b"SQLite format 3"), "{file} is not encrypted");
    }
}
