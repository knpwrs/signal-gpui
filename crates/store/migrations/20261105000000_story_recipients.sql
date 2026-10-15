-- Ours: JSON list of who each story went to, with the story lists that
-- sent it to them (Desktop's sendStateByConversationId and
-- storyDistributionListId), to delete it for everyone later.
ALTER TABLE stories ADD COLUMN recipients TEXT;
