-- Add a `state_key` column to the events table, so we can efficiently query
-- for state events (i.e. events with a `state_key`, even an empty one).
--
-- Backfilling this value for existing events would require reprocessing each
-- event, so we empty the event cache before adding the new column instead.

DELETE FROM "linked_chunks";
DELETE FROM "event_chunks"; -- should be done by cascading
DELETE FROM "gap_chunks"; -- should be done by cascading
DELETE FROM "threads";
DELETE FROM "events";

ALTER TABLE "events" ADD COLUMN
    -- The (possibly) hashed `state_key` of the event. It is `NULL` if the event
    -- has no `state_key`, i.e. it isn't a state event.
    "state_key" BLOB NULL
;

-- Index to support efficient queries filtering by state key,
-- e.g. `WHERE room_id = ? AND state_key = ?`.
CREATE INDEX "events_state_key_idx"
    ON "events" ("room_id", "state_key");

-- Index to support efficient queries filtering by state key and timestamp,
-- e.g. `WHERE room_id = ? AND state_key IS NULL AND timestamp < ?` or
-- `WHERE room_id = ? AND state_key = ? AND timestamp < ?`.
CREATE INDEX "events_state_key_timestamp_idx"
    ON "events" ("room_id", "state_key", "timestamp");
