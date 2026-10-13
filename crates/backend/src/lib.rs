//! Signal networking on a dedicated Tokio runtime.
//!
//! The UI never touches presage directly: it sends [`Command`]s and receives
//! [`Event`]s, and reads rendered state from [`signal_store::AppDb`].

use std::{future::Future, path::PathBuf};

use futures::channel::mpsc;
use signal_store::{AppDb, ConversationId};

pub use url::Url;

#[derive(Debug, Clone)]
pub struct Config {
    /// Directory holding `signal.db` (presage protocol state), `app.db` and attachments.
    pub data_dir: PathBuf,
    /// Raw SQLCipher key for both databases.
    pub db_key: [u8; 32],
    /// Device name shown in the phone's "Linked devices" list.
    pub device_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Starts linking as a secondary device. Emits `Event::LinkUrl` then `Event::Linked`.
    Link,
    SendText {
        conversation: ConversationId,
        body: String,
        /// Message id (in `AppDb`) being replied to.
        quote: Option<i64>,
    },
    /// Reacts to message `message_id`; `remove` retracts our reaction.
    React { message_id: i64, emoji: String, remove: bool },
    /// Marks a conversation read locally and sends read receipts / sync.
    MarkRead { conversation: ConversationId },
    /// Sends a typing indicator.
    Typing { conversation: ConversationId, started: bool },
    /// Deletes our own message for everyone.
    DeleteForEveryone { message_id: i64 },
    /// Downloads attachment `index` of `message_id`.
    FetchAttachment { message_id: i64, index: usize },
    /// Unlinks this device and wipes local state.
    Unlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Offline,
    Connecting,
    /// Connected and draining the server queue.
    Syncing,
    Online,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// No account on disk; the UI should show the link screen.
    NeedsLinking,
    /// Provisioning URL to render as a QR code.
    LinkUrl(Url),
    LinkFailed(String),
    /// Account is ready. Emitted at startup when already linked, and after linking.
    Ready { phone: Option<String>, aci: String },
    Connection(ConnectionState),
    /// Conversation list rows changed (title, preview, unread, ...).
    ConversationsChanged,
    /// A message was inserted or updated (status, reaction, edit, delete, attachment).
    MessageChanged { conversation: ConversationId, message_id: i64 },
    Typing { conversation: ConversationId, author: String, started: bool },
    Error(String),
    Unlinked,
}

/// Handle to the running backend. Cheap to clone.
#[derive(Clone)]
pub struct Backend {
    db: AppDb,
    commands: mpsc::UnboundedSender<Command>,
    runtime: tokio::runtime::Handle,
}

impl Backend {
    /// Opens the databases, starts the runtime thread, and begins connecting
    /// (or emits `NeedsLinking`). Events arrive on the returned receiver, which
    /// may be polled from any executor.
    pub fn start(config: Config) -> anyhow::Result<(Backend, mpsc::UnboundedReceiver<Event>)> {
        todo!()
    }

    pub fn db(&self) -> &AppDb {
        &self.db
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.unbounded_send(command);
    }

    /// Runs `fut` on the backend runtime (needed for `AppDb` queries) and
    /// returns a future that can be awaited from any executor.
    pub fn spawn<F>(&self, fut: F) -> impl Future<Output = F::Output> + Send + 'static
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = self.runtime.spawn(fut);
        async move { handle.await.expect("backend task panicked") }
    }
}
