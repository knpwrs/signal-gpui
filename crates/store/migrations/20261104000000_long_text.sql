-- A message whose whole text came as a long-text attachment (Desktop's
-- bodyAttachment) and isn't in `body` yet: JSON LongText. Cleared once the
-- text replaces the body.
ALTER TABLE messages ADD COLUMN long_text TEXT;
CREATE INDEX messages_long_text ON messages (id) WHERE long_text IS NOT NULL;
