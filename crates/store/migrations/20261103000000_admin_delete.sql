-- Who deleted a message for everyone as a group admin (Desktop's
-- deletedForEveryoneByAdminAci): JSON DeletedByAdmin. NULL when its author
-- deleted it, or it isn't deleted.
ALTER TABLE messages ADD COLUMN deleted_by_admin TEXT;
