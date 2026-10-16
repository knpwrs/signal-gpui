-- An edited message's earlier versions, oldest first (Desktop's
-- `editHistory` past the current one): a JSON list of `EditRevision`, each
-- with the timestamp that named it, its text, mentions and formatting. NULL
-- until the first edit.
ALTER TABLE messages ADD COLUMN edit_history TEXT;
