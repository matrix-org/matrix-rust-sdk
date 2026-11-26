-- Add an index on (room_id, session_id), so the event cache redecryptor spends
-- less time refreshing encryption info, which would otherwise block room-list
-- sync processing long enough to cause issues.
CREATE INDEX "events_room_session_idx"
    ON "events" ("room_id", "session_id");
