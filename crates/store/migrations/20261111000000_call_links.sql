-- Call links (Desktop's `callLinks` table): the link's root key (which
-- names its room, `room_id` in hex), our admin passkey if we made it, and
-- the state the calling server last gave (name, approval setting,
-- revoked, expiration in unix millis). `deleted_at` (unix millis) marks a
-- link we deleted until Storage Service knows.
CREATE TABLE call_links (
    room_id TEXT PRIMARY KEY,
    root_key BLOB NOT NULL,
    admin_key BLOB,
    name TEXT NOT NULL DEFAULT '',
    restrictions INTEGER NOT NULL DEFAULT 2,
    revoked INTEGER NOT NULL DEFAULT 0,
    expiration INTEGER,
    deleted_at INTEGER,
    added_at INTEGER NOT NULL
);

-- Calls through call links (Desktop's `callsHistory` rows with mode
-- 'Adhoc'): one per room and call (RingRTC's id from the call's era, its
-- bits as an i64). `status` is Desktop's AdhocCallStatus: 'pending' for a
-- link we made and haven't used, 'joined', or 'generic' (seen, not joined).
CREATE TABLE adhoc_calls (
    room_id TEXT NOT NULL,
    call_id INTEGER NOT NULL,
    at INTEGER NOT NULL,
    status TEXT NOT NULL,
    PRIMARY KEY (room_id, call_id)
);
CREATE INDEX adhoc_calls_at ON adhoc_calls (at);
