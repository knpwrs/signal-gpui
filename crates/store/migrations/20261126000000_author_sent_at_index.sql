-- A receipt, reaction or read sync names its message by author and time.
-- With `messages_author` on the author alone, each of those lookups read
-- every message by that author (every one we sent, for a receipt): 2 to 5
-- seconds apiece on a real account. The time beside the author makes it one
-- seek, and renaming an author still finds their messages by the prefix.
DROP INDEX IF EXISTS messages_author;
CREATE INDEX messages_author ON messages (author_key, sent_at);
