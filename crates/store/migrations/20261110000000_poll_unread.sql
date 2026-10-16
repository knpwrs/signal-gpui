-- Someone else voted in our poll since we last read its chat (Desktop's
-- hasUnreadPollVotes, its migration 1520-poll-votes-unread).
ALTER TABLE messages ADD COLUMN unread_poll_votes INTEGER NOT NULL DEFAULT 0;
CREATE INDEX messages_unread_poll_votes ON messages (conversation_id) WHERE unread_poll_votes = 1;
-- Polls, to find our votes still sending when the app starts.
CREATE INDEX messages_polls ON messages (id) WHERE poll IS NOT NULL;
