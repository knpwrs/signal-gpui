-- Renaming an author (rename_authors) finds their messages and the quotes
-- of them; without these, each rename scanned every message while holding
-- the write lock.
CREATE INDEX IF NOT EXISTS messages_author ON messages (author_key);
CREATE INDEX IF NOT EXISTS messages_quote_author ON messages (json_extract(quote, '$.author')) WHERE quote IS NOT NULL;
