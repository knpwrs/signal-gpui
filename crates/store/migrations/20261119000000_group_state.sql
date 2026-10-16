-- A group's administration state beyond its members, as JSON
-- (`GroupState`): announcement-only, who may edit details and add people,
-- the group link, invitations, requests to join, bans, and whether we're a
-- member, invited or asking to join. NULL for 1:1 chats and groups not read
-- since. Desktop keeps these on the conversation (accessControl,
-- announcementsOnly, groupInviteLinkPassword, pendingMembersV2,
-- pendingAdminApprovalV2, bannedMembersV2).
ALTER TABLE conversations ADD COLUMN group_state TEXT;
