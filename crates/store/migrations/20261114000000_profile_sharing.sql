-- Desktop's `profileSharing`: the people (and groups) we share our profile
-- with, by conversation id, whether or not there's a chat with them. Set by
-- Storage Service (`whitelisted`), the backup's `profileSharing`, and by
-- sending them a message. Desktop takes one-to-one stories only from them
-- (`MessageReceiver`: "Dropping story message - !profileSharing").
CREATE TABLE profile_sharing (
    id TEXT PRIMARY KEY NOT NULL
) STRICT;

-- A group's `storySendMode` (Storage Service's GroupV2Record and the
-- backup's Group): 1 never (DISABLED), 2 always (ENABLED). A group not
-- listed has the default (Desktop's IfActive: listed while it has stories).
CREATE TABLE story_send_modes (
    id   TEXT PRIMARY KEY NOT NULL,
    mode INTEGER NOT NULL
) STRICT;
