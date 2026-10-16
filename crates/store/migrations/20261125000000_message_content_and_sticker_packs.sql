-- What a message holds in place of text (Desktop's `contact`, `payment` and
-- `giftBadge` message attributes): a JSON `MessageContent`, NULL for most.
ALTER TABLE messages ADD COLUMN content TEXT;

-- Sticker packs we know of (Desktop's `sticker_packs`): from stickers we
-- received, the Signal Artist Series, and ones installed; the stickers of
-- installed packs are kept by presage. `position` orders installed packs;
-- `uninstalled_at` remembers an uninstall for Storage Service.
CREATE TABLE sticker_packs (
    -- hex pack id and key
    id             TEXT PRIMARY KEY,
    key            TEXT NOT NULL,
    title          TEXT,
    author         TEXT,
    -- the downloaded cover image
    cover_path     TEXT,
    sticker_count  INTEGER NOT NULL DEFAULT 0,
    installed      INTEGER NOT NULL DEFAULT 0,
    position       INTEGER,
    uninstalled_at INTEGER,
    created_at     INTEGER NOT NULL
) STRICT;
