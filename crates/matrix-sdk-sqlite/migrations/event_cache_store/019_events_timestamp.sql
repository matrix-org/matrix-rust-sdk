-- Add timestamp to the events table so we can efficiently query for
-- expired events by timestamp (e.g. for MSC1763 retention policy enforcement).
--
-- Backfilling the timestamp for existing events would require reprocessing each
-- event, so we empty the event cache before adding the new column instead.

DELETE FROM "linked_chunks";
DELETE FROM "event_chunks"; -- should be done by cascading
DELETE FROM "gap_chunks"; -- should be done by cascading
DELETE FROM "threads";
DELETE FROM "events";

ALTER TABLE "events" ADD COLUMN
    -- The timestamp is in milliseconds since Unix epoch and comes from
    -- `TimelineEvent::timestamp()`.
    "timestamp" INTEGER
;

-- Index to support efficient queries WHERE room_id = ? AND timestamp < ?
CREATE INDEX "events_timestamp_idx"
    ON "events" ("room_id", "timestamp");
