-- A view-once message (Desktop's isViewOnce, isErased, readStatus Viewed,
-- isTapToViewInvalid): NULL for an ordinary message; 0 while its media is
-- here to view once; 1 once viewed (its files deleted); 2 once its media
-- went without being viewed (too old, or ours once sent); 3 when it isn't a
-- valid view-once message (nothing to view).
ALTER TABLE messages ADD COLUMN view_once INTEGER;
-- Desktop's `messages_view_once` index: the ones still to erase, oldest
-- first (tapToViewMessagesDeletionService).
CREATE INDEX messages_view_once ON messages (received_at) WHERE view_once = 0;
