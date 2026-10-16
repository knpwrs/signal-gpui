//! Chat folders, as Desktop syncs them: each change saved here and as the
//! folder's `ChatFolderRecord` (`ts/state/ducks/chatFolders.preload.ts`,
//! which runs a storage upload after each), records read back with
//! `mergeChatFolderRecord`, and an "All chats" folder made after a sync
//! that finds none (`ts/services/storage.preload.ts`).

use signal_store::{ChatFolder, ChatFolderEdit};

use super::*;

impl Actor {
    pub(super) async fn edit_chat_folders(self: &Rc<Self>, edit: ChatFolderEdit) {
        let changed = match self.db.edit_chat_folders(edit).await {
            Ok(changed) => changed,
            Err(error) => return self.error("failed to save chat folders", error),
        };
        if changed.is_empty() {
            return;
        }
        self.emit(Event::ChatFoldersChanged);
        self.upload_chat_folders(changed).await;
    }

    /// Saves these folders' records, one at a time. One that fails is
    /// reported, and the rest wait for the next change.
    async fn upload_chat_folders(self: &Rc<Self>, ids: Vec<String>) {
        let Some(manager) = self.manager() else { return };
        let folders: Vec<ChatFolder> = match self.db.chat_folders().await {
            Ok(folders) => folders.into_iter().filter(|f| ids.contains(&f.id)).collect(),
            Err(error) => return self.error("failed to read chat folders", error),
        };
        for folder in folders {
            if let Err(error) = storage::write_chat_folder(&manager, &folder).await {
                return self.error("couldn't save the chat folder on your other devices", format!("{error:#}"));
            }
        }
        self.storage_written();
    }

    /// Storage Service's folders merged, then an "All chats" folder if
    /// none is left (saved for every device).
    pub(super) async fn apply_storage_chat_folders(self: &Rc<Self>, remote: Vec<ChatFolder>) {
        let mut changed = false;
        for folder in remote {
            match self.db.edit_chat_folders(ChatFolderEdit::Merge(folder)).await {
                Ok(ids) => changed |= !ids.is_empty(),
                Err(error) => return self.error("failed to save chat folders", error),
            }
        }
        let ensure = ChatFolderEdit::EnsureAll { id: uuid::Uuid::new_v4().to_string() };
        let made = match self.db.edit_chat_folders(ensure).await {
            Ok(ids) => ids,
            Err(error) => return self.error("failed to save chat folders", error),
        };
        if changed || !made.is_empty() {
            self.emit(Event::ChatFoldersChanged);
        }
        if !made.is_empty() {
            self.upload_chat_folders(made).await;
        }
    }
}
