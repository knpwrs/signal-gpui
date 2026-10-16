-- Who each of our messages went to and how far it got with each, as
-- Signal Desktop keeps `sendStateByConversationId`
-- (`ts/messages/MessageSendState.std.ts`): status 0 failed, 1 pending,
-- 2 sent, 3 delivered, 4 read, 5 viewed, 6 skipped, with when it last
-- changed (unix millis), and why a send to them failed.
CREATE TABLE message_send_states (
    message_id INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    recipient  TEXT NOT NULL,
    status     INTEGER NOT NULL,
    updated_at INTEGER,
    error      TEXT,
    PRIMARY KEY (message_id, recipient)
) STRICT;

-- An incoming voice message played here or on another device (Desktop's
-- `ReadStatus.Viewed`), so its viewed receipt goes once.
ALTER TABLE messages ADD COLUMN viewed INTEGER NOT NULL DEFAULT 0;
