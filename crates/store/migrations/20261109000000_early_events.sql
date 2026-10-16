-- Deletes, edits, reactions, votes, receipts and syncs that came before the
-- message they're for, kept until it comes (Desktop keeps their envelopes
-- in `unprocessed`, or a `syncTasks` row, until applied). `action` is the
-- backend's JSON; `target_author` is '' for us.
CREATE TABLE early_events (
    id INTEGER PRIMARY KEY,
    target_author TEXT NOT NULL,
    target_sent_at INTEGER NOT NULL,
    -- 1: kept as Desktop keeps a sync task, else as an envelope.
    sync INTEGER NOT NULL,
    action TEXT NOT NULL,
    received_at INTEGER NOT NULL,
    -- Times loaded at startup.
    attempts INTEGER NOT NULL DEFAULT 0
);
-- Also makes a re-delivered event a no-op.
CREATE UNIQUE INDEX early_events_target ON early_events (target_sent_at, target_author, action);
