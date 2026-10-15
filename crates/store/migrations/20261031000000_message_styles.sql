-- A message's formatting (bold, italic, spoilers…): JSON StyleSpans over the
-- body's bytes, as mentions are kept.
ALTER TABLE messages ADD COLUMN styles TEXT;
