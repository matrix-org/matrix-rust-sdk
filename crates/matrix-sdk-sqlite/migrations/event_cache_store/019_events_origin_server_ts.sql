-- Add origin_server_ts to the events table so we can efficiently query for
-- expired events by timestamp (e.g. for MSC1763 retention policy enforcement).
--
-- Backfilling the timestamp for existing events would require reprocessing each
-- event, so we empty the event cache and recreate the table with the new column
-- instead. This follows the same approach as migration 012.

DELETE FROM "linked_chunks";
DELETE FROM "event_chunks"; -- should be done by cascading
DELETE FROM "gap_chunks"; -- should be done by cascading
DELETE FROM "threads";
DELETE FROM "events";

DROP TABLE "events";

CREATE TABLE "events" (
    -- The room in which the event is located.
    "room_id" BLOB NOT NULL,

    -- The `OwnedEventId` of this event.
    "event_id" BLOB NOT NULL,

    -- The event type of this event.
    "event_type" BLOB NOT NULL,

    -- The ID of the session that was used to encrypt this event, may be null if
    -- the event wasn't encrypted.
    "session_id" BLOB NULL,

    -- JSON serialized `TimelineEvent` (encrypted value).
    "content" BLOB NOT NULL,

    -- If this event is an aggregation (related event), the event id of the event it relates to.
    -- Can be null if this event isn't an aggregation.
    "relates_to" BLOB,

    -- If this event is an aggregation (related event), the kind of relation it has to the event it
    -- relates to.
    -- Can be null if this event isn't an aggregation.
    "rel_type" BLOB,

    -- The origin_server_ts of this event in milliseconds since Unix epoch.
    -- Used for MSC1763 retention policy enforcement. Null if the event has no
    -- known timestamp (e.g. its origin_server_ts could not be parsed).
    -- Such events are left untouched by retention
    -- purging: SQL's `<` comparison against NULL is never true, so they're
    -- naturally excluded from `DELETE ... WHERE origin_server_ts < ?`.
    "origin_server_ts" INTEGER,

    -- Primary key is the event ID.
    PRIMARY KEY (event_id)
)
WITHOUT ROWID;

-- Add an index to speed up queries that look for related events in a room.
-- Carried forward from migration 012.
CREATE INDEX "relates_to_idx"
    ON "events" ("room_id", "relates_to");

-- Add an index to speed up queries that look for related events in a room, with an additional
-- filter. Carried forward from migration 012.
CREATE INDEX "relates_to_rel_type_idx"
    ON "events" ("room_id", "relates_to", "rel_type");

-- Add an index to speed up queries that look for related events in a room.
-- Carried forward from migration 012.
CREATE INDEX "event_type_index"
    ON "events" ("room_id", "event_type", "session_id");

-- Index to support efficient queries WHERE room_id = ? AND origin_server_ts < ?
CREATE INDEX "events_origin_server_ts_idx"
    ON "events" ("room_id", "origin_server_ts");
