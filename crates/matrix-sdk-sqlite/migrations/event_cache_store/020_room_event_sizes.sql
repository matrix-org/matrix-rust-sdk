-- Per-room byte counters for the events table, kept up to date by triggers,
-- so that measuring the storage usage doesn't scan the (large) events table.
-- Filled once, lazily, from the existing rows (see the kv key
-- `room_event_sizes_ready`).
CREATE TABLE "room_event_sizes" (
    "room_id" BLOB PRIMARY KEY NOT NULL,
    "bytes" INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TRIGGER "events_size_insert" AFTER INSERT ON "events" BEGIN
    INSERT INTO "room_event_sizes" ("room_id", "bytes")
        VALUES (NEW.room_id, LENGTH(NEW.room_id) + LENGTH(NEW.event_id) + LENGTH(NEW.event_type) + COALESCE(LENGTH(NEW.session_id), 0) + LENGTH(NEW.content) + COALESCE(LENGTH(NEW.relates_to), 0) + COALESCE(LENGTH(NEW.rel_type), 0) + COALESCE(LENGTH(NEW.msgtype), 0))
        ON CONFLICT ("room_id") DO UPDATE SET "bytes" = "bytes" + excluded."bytes";
END;

CREATE TRIGGER "events_size_delete" AFTER DELETE ON "events" BEGIN
    UPDATE "room_event_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD.room_id) + LENGTH(OLD.event_id) + LENGTH(OLD.event_type) + COALESCE(LENGTH(OLD.session_id), 0) + LENGTH(OLD.content) + COALESCE(LENGTH(OLD.relates_to), 0) + COALESCE(LENGTH(OLD.rel_type), 0) + COALESCE(LENGTH(OLD.msgtype), 0))
        WHERE "room_id" = OLD.room_id;
END;

CREATE TRIGGER "events_size_update" AFTER UPDATE ON "events" BEGIN
    UPDATE "room_event_sizes"
        SET "bytes" = "bytes"
            - (LENGTH(OLD.room_id) + LENGTH(OLD.event_id) + LENGTH(OLD.event_type) + COALESCE(LENGTH(OLD.session_id), 0) + LENGTH(OLD.content) + COALESCE(LENGTH(OLD.relates_to), 0) + COALESCE(LENGTH(OLD.rel_type), 0) + COALESCE(LENGTH(OLD.msgtype), 0))
            + (LENGTH(NEW.room_id) + LENGTH(NEW.event_id) + LENGTH(NEW.event_type) + COALESCE(LENGTH(NEW.session_id), 0) + LENGTH(NEW.content) + COALESCE(LENGTH(NEW.relates_to), 0) + COALESCE(LENGTH(NEW.rel_type), 0) + COALESCE(LENGTH(NEW.msgtype), 0))
        WHERE "room_id" = NEW.room_id;
END;
