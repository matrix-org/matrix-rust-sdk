-- Add an `is_state_event` column to the events table, so we can efficiently
-- query for state events (i.e. events with a `state_key`, even an empty one).
--
-- Backfilling this value for existing events would require reprocessing each
-- event, so we empty the event cache before adding the new column instead.

DELETE FROM "linked_chunks";
DELETE FROM "event_chunks"; -- should be done by cascading
DELETE FROM "gap_chunks"; -- should be done by cascading
DELETE FROM "threads";
DELETE FROM "events";

ALTER TABLE "events" ADD COLUMN
    -- Whether the event has a `state_key` field.
    -- Stored as 0 (false) or 1 (true).
    "is_state_event" BOOLEAN NOT NULL DEFAULT FALSE
;
