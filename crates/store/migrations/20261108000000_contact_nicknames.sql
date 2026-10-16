-- Nicknames and notes we gave contacts (Storage Service ContactRecord
-- `nickname` and `note`, Desktop's nicknameGivenName, nicknameFamilyName and
-- note), keyed by ACI service id, with the name they'd have without the
-- nickname (address-book or profile name from the same record) for
-- Desktop's "Nickname (profile name)". Rows for contacts with a nickname or
-- a note only; replaced wholesale with contact_names on each sync.
CREATE TABLE contact_nicknames (
    service_id TEXT PRIMARY KEY NOT NULL,
    given      TEXT NOT NULL,
    family     TEXT NOT NULL,
    note       TEXT NOT NULL,
    plain_name TEXT
) STRICT;
