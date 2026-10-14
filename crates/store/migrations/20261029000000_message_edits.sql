-- Each edit's own timestamp. Reactions, replies and receipts can name an
-- edited message by any of them (Desktop's `editHistory` timestamps), not
-- only by the original's `sent_at`.
CREATE TABLE message_edits (
    message_id INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    sent_at    INTEGER NOT NULL,
    PRIMARY KEY (message_id, sent_at)
) STRICT;

CREATE INDEX message_edits_sent_at ON message_edits (sent_at);
