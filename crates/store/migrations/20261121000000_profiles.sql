-- What people's profiles said when last fetched, as Signal Desktop keeps
-- them on a conversation (`profileName`, `profileFamilyName`, `about`,
-- `aboutEmoji`, `badges`), and their username from their Storage Service
-- contact record. Ours is here too, under our ACI. (Whether they're in our
-- address book is `system_contacts`.)
CREATE TABLE profiles (
    service_id  TEXT PRIMARY KEY NOT NULL,
    given_name  TEXT NOT NULL DEFAULT '',
    family_name TEXT NOT NULL DEFAULT '',
    about       TEXT NOT NULL DEFAULT '',
    about_emoji TEXT NOT NULL DEFAULT '',
    -- JSON list of ProfileBadge: the badges their profile shows.
    badges      TEXT NOT NULL DEFAULT '[]',
    -- Unix millis of the profile fetch; 0 when never fetched.
    fetched_at  INTEGER NOT NULL DEFAULT 0,
    -- Their username, from the contact record.
    username    TEXT
) STRICT;

-- Badges by id, as the server describes them in profiles (Desktop's
-- `badges` table): JSON lists of image URLs per size.
CREATE TABLE badges (
    id          TEXT PRIMARY KEY NOT NULL,
    category    TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL,
    images      TEXT NOT NULL
) STRICT;

-- Downloaded badge images by URL (Desktop's `badgeImageFiles`).
CREATE TABLE badge_image_files (
    url  TEXT PRIMARY KEY NOT NULL,
    path TEXT NOT NULL
) STRICT;
