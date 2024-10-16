-- New send queue events, now persists the type of error causing it to be wedged
ALTER TABLE "send_queue_events"
    -- Used as a value, thus encrypted/decrypted
    ADD COLUMN "wedge_reason" BLOB;
