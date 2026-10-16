-- Who a message came from or went to by sealed sender (a JSON list of
-- service ids; Desktop's `unidentifiedDeliveryReceived` and
-- `unidentifiedDeliveries`), for its status icon in message details.
ALTER TABLE messages ADD COLUMN sealed_sender TEXT;
