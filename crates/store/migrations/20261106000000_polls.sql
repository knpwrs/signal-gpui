-- A poll (Desktop's `poll` message attribute): JSON Poll, its question,
-- options and votes. The question is also the message's body, so search
-- finds it.
ALTER TABLE messages ADD COLUMN poll TEXT;
