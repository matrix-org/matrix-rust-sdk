-- After the merge of https://github.com/matrix-org/matrix-rust-sdk/pull/5648,
-- we want all events to get a `TimelineEvent::timestamp` value (extracted from
-- `origin_server_ts`).
--
-- To accomplish that, we are emptying the event cache. New synced events will
-- be built correctly, with a valid `TimelineEvent::timestamp`, allowing a
-- clear, stable situation.

DELETE from linked_chunks;
DELETE from event_chunks; -- should be done by cascading
DELETE from gap_chunks; -- should be done by cascading
DELETE from events;

DROP TABLE events;

-- Events and their content.
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

    -- Primary key is the event ID.
    PRIMARY KEY (event_id)
)
WITHOUT ROWID;
