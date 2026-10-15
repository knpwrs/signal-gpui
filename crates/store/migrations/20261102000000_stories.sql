-- Stories: 24-hour posts, kept apart from chat messages (Desktop keeps
-- them as messages of type 'story', excluded from timelines and search;
-- a table of their own keeps them out of ours).
CREATE TABLE stories (
    id              INTEGER PRIMARY KEY,
    -- The author's service id; '' for ours.
    author_key      TEXT NOT NULL,
    author_name     TEXT,
    sent_at         INTEGER NOT NULL,
    received_at     INTEGER NOT NULL,
    -- A group story's conversation id; NULL for one to the author's
    -- connections.
    group_id        TEXT,
    -- JSON StoryContent: a text story, or an attachment with a caption.
    content         TEXT NOT NULL,
    -- The attachment pointer, protobuf bytes, to download it from.
    pointer         BLOB,
    allows_replies  INTEGER NOT NULL DEFAULT 0,
    -- Viewed here (or on another of our devices).
    is_read         INTEGER NOT NULL DEFAULT 0,
    expires_at      INTEGER NOT NULL,
    -- JSON StoryReplies (a group story's replies and reactions).
    replies         TEXT,
    -- Ours: JSON list of who viewed it.
    views           TEXT,
    UNIQUE (author_key, sent_at)
);
CREATE INDEX stories_expires ON stories (expires_at);
