CREATE TABLE conversations (
    id                   TEXT PRIMARY KEY NOT NULL,
    kind                 TEXT NOT NULL,
    title                TEXT NOT NULL,
    phone                TEXT,
    color                INTEGER NOT NULL DEFAULT 0,
    -- Local path of a downloaded, decrypted profile or group avatar.
    avatar_path          TEXT,
    -- A group's member ACIs (JSON), for Signal Desktop's per-member name colors.
    members              TEXT,
    last_message_preview TEXT,
    last_message_at      INTEGER NOT NULL DEFAULT 0,
    unread_count         INTEGER NOT NULL DEFAULT 0,
    pinned               INTEGER NOT NULL DEFAULT 0,
    -- Position in the account's pinned list (Storage Service); lower sorts first.
    pin_order            INTEGER NOT NULL DEFAULT 0,
    -- Signal mutes until a time (unix millis); i64::MAX means "always".
    muted_until          INTEGER NOT NULL DEFAULT 0,
    archived             INTEGER NOT NULL DEFAULT 0,
    -- Storage Service chat flags: "Mark as unread" and blocked.
    marked_unread        INTEGER NOT NULL DEFAULT 0,
    blocked              INTEGER NOT NULL DEFAULT 0,
    draft                TEXT,
    expire_timer         INTEGER NOT NULL DEFAULT 0,
    -- The 1:1 timer's version (Signal's expireTimerVersion; 1 is the initial
    -- version).
    expire_timer_version INTEGER NOT NULL DEFAULT 1,
    -- Whether we may change a group's details (timer, pins, name); always for
    -- 1:1 chats. Refreshed from the group's access control.
    can_edit_info        INTEGER NOT NULL DEFAULT 1,
    -- We're no longer in this group (left or removed).
    left_group           INTEGER NOT NULL DEFAULT 0,
    -- A group's description and its admins (JSON list of ACIs), for Group settings.
    description          TEXT,
    admins               TEXT,
    -- Group member labels (JSON: ACI -> {emoji, text}) and whether we may set ours.
    member_labels        TEXT,
    can_label            INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX conversations_inbox ON conversations (archived, pinned DESC, last_message_at DESC);

CREATE TABLE messages (
    id              INTEGER PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    sent_at         INTEGER NOT NULL,
    received_at     INTEGER NOT NULL,
    -- 0 = incoming, 1 = outgoing
    direction       INTEGER NOT NULL,
    author          TEXT,
    -- author, or '' for our own messages; part of the cross-device identity
    author_key      TEXT NOT NULL,
    author_name     TEXT,
    body            TEXT NOT NULL,
    -- 0 sending, 1 sent, 2 delivered, 3 read, 4 failed
    status          INTEGER NOT NULL,
    reactions       TEXT NOT NULL DEFAULT '[]',
    quote           TEXT,
    attachments     TEXT NOT NULL DEFAULT '[]',
    -- A message's link preview (JSON: url, title, description, date, image).
    link_preview    TEXT,
    -- Where each @mention sits in the (rendered) body: a JSON list of
    -- `MentionSpan`s, byte offsets into `body`.
    mentions        TEXT,
    -- Timeline notices ("You set the disappearing message time to 1 day",
    -- "Alice pinned a message"): a JSON `Notice`, with an empty body.
    notice          TEXT,
    deleted         INTEGER NOT NULL DEFAULT 0,
    edited          INTEGER NOT NULL DEFAULT 0,
    -- How many times we've edited a message (Signal allows 10).
    edit_count      INTEGER NOT NULL DEFAULT 0,
    expires_in      INTEGER NOT NULL DEFAULT 0,
    -- When the disappearing countdown started (outgoing: when sent;
    -- incoming: when read).
    expire_start    INTEGER,
    is_read         INTEGER NOT NULL DEFAULT 1
) STRICT;

CREATE UNIQUE INDEX messages_identity ON messages (conversation_id, author_key, sent_at);
CREATE INDEX messages_timeline ON messages (conversation_id, sent_at, id);
CREATE INDEX messages_unread ON messages (conversation_id, is_read) WHERE is_read = 0;
CREATE INDEX messages_outgoing_sent_at ON messages (sent_at) WHERE author_key = '';
CREATE INDEX messages_expiry ON messages (expire_start + expires_in * 1000) WHERE expire_start IS NOT NULL;

CREATE VIRTUAL TABLE messages_fts USING fts5 (
    body,
    content = 'messages',
    content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, body) VALUES (new.id, new.body);
END;

CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
END;

CREATE TRIGGER messages_fts_update AFTER UPDATE OF body ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, body) VALUES ('delete', old.id, old.body);
    INSERT INTO messages_fts (rowid, body) VALUES (new.id, new.body);
END;

-- Signal's pinned messages, at most three per chat.
CREATE TABLE pinned_messages (
    conversation_id TEXT NOT NULL,
    message_id      INTEGER NOT NULL,
    pinned_at       INTEGER NOT NULL,
    -- Unix millis; NULL pins forever.
    expires_at      INTEGER,
    PRIMARY KEY (conversation_id, message_id)
) STRICT;

-- Display names from the account's Storage Service contact records (nickname,
-- address-book name or profile name), keyed by ACI service id. Replaced
-- wholesale on each sync.
CREATE TABLE contact_names (
    service_id TEXT PRIMARY KEY NOT NULL,
    name       TEXT NOT NULL
) STRICT;

-- Everyone and every group the account knows, conversation or not, for
-- search and starting chats (Signal Desktop lists these too).
CREATE TABLE directory (
    id    TEXT PRIMARY KEY NOT NULL,  -- conversation id
    kind  TEXT NOT NULL,
    title TEXT NOT NULL,
    phone TEXT
) STRICT;

-- Downloaded profile photos by service id: anyone whose avatar we show,
-- including group members without a 1:1 chat.
CREATE TABLE person_avatars (
    service_id TEXT PRIMARY KEY NOT NULL,
    path       TEXT NOT NULL
) STRICT;

CREATE TABLE emoji_usage (
    emoji     TEXT PRIMARY KEY NOT NULL,
    count     INTEGER NOT NULL,
    last_used INTEGER NOT NULL
) STRICT;

CREATE TABLE settings (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;
