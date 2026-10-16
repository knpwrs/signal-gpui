-- Whether we may add members to a group: its member access is "any member",
-- or we're an admin (Desktop's canAddNewMembers).
ALTER TABLE conversations ADD COLUMN can_add_members INTEGER NOT NULL DEFAULT 0;
-- An admin ended the group for everyone (Desktop's `terminated`): no more
-- sends, calls or deletes for everyone in it.
ALTER TABLE conversations ADD COLUMN group_ended INTEGER NOT NULL DEFAULT 0;
