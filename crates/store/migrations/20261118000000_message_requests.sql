-- Message requests, as Signal Desktop keeps them
-- (`ts/util/isConversationAccepted.preload.ts`).
--
-- Desktop's `messageRequestResponseType`: our last answer to the chat's
-- request, as `SyncMessage.MessageRequestResponse.Type` numbers it (0 none,
-- 1 accept, 2 delete, 3 block, 4 block and delete, 5 spam, 6 block and
-- spam).
ALTER TABLE conversations ADD COLUMN message_request INTEGER NOT NULL DEFAULT 0;
-- We reported it as spam (Desktop's `isReported`).
ALTER TABLE conversations ADD COLUMN reported INTEGER NOT NULL DEFAULT 0;
-- Who added us to the group outright (an ACI, Desktop's `addedBy`). Who
-- invited us is the group state's membership.
ALTER TABLE conversations ADD COLUMN added_by TEXT;
-- The server's GUID for an incoming message (Desktop's `serverGuid`), which
-- a spam report names.
ALTER TABLE messages ADD COLUMN server_guid TEXT;
-- People in our address book (Desktop's `isInSystemContacts`): a Storage
-- Service contact record or the phone's contact sync names them.
CREATE TABLE system_contacts (
    service_id TEXT PRIMARY KEY NOT NULL
) STRICT;
