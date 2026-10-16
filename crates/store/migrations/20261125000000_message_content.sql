-- What a message holds in place of text (Desktop's `contact`, `payment` and
-- `giftBadge` message attributes): a JSON `MessageContent`, NULL for most.
ALTER TABLE messages ADD COLUMN content TEXT;
