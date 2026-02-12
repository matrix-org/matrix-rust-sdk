-- SQLite versions older than 3.35 don't support DROP COLUMN, so rebuild the table
-- without the `wedged` column.
CREATE TABLE "send_queue_events_new" (
    -- This is used as a key, thus hashed.
    "room_id" BLOB NOT NULL,

    -- This is used as a value (thus encrypted/decrypted).
    "room_id_val" BLOB NOT NULL,

    -- This is used as both a key and a value, thus neither encrypted/decrypted/hashed.
    "transaction_id" BLOB NOT NULL,

    -- Used as a value, thus encrypted/decrypted.
    "content" BLOB NOT NULL,

    -- The serialized json (bytes) representing the error. Used as a value, thus encrypted/decrypted.
    -- NULLABLE field (default NULL)
    "wedge_reason" BLOB,

    PRIMARY KEY ("room_id", "transaction_id")
);

INSERT INTO "send_queue_events_new" (
    "room_id",
    "room_id_val",
    "transaction_id",
    "content",
    "wedge_reason"
)
SELECT
    "room_id",
    "room_id_val",
    "transaction_id",
    "content",
    "wedge_reason"
FROM "send_queue_events";

DROP TABLE "send_queue_events";
ALTER TABLE "send_queue_events_new" RENAME TO "send_queue_events";
