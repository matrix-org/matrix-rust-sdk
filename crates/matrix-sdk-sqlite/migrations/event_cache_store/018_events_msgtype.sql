-- Index the `msgtype` of room messages, so that a message-type filtered view
-- of a room (e.g. its media and files) can be built without decoding every
-- event of the room.
--
-- The column holds the hashed `msgtype` of `m.room.message` events (a hashed
-- empty string when the message has none), NULL for other events. Existing
-- rows are left NULL: they are backfilled lazily, room by room, the first time
-- a message-type query runs for the room.
ALTER TABLE "events" ADD COLUMN "msgtype" BLOB NULL;

CREATE INDEX "events_room_msgtype_idx"
    ON "events" ("room_id", "msgtype");

-- `get_room_events(room_id, event_type: None, session_id: Some(_))` (the
-- redecryptor's per-session encryption-info refresh, run once per received
-- room key) filtered on `session_id` without an index prefix covering it:
-- `event_type_index` is (room_id, event_type, session_id), so it degraded to a
-- scan of every event of the room (~1.8s per key in a large room, serialising
-- all other store access behind it).
CREATE INDEX "events_room_session_idx"
    ON "events" ("room_id", "session_id");
