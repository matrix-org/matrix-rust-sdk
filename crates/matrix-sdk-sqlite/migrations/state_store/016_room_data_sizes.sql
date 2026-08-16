-- Per-room byte counters for the room data tables, kept up to date by
-- triggers, so that measuring the storage usage doesn't scan the tables.
-- Filled once, lazily, from the existing rows (see the kv key
-- `room_data_sizes_ready`).
CREATE TABLE "room_data_sizes" (
    "table_name" TEXT NOT NULL,
    "room_id" BLOB NOT NULL,
    "bytes" INTEGER NOT NULL,
    PRIMARY KEY ("table_name", "room_id")
) WITHOUT ROWID;

CREATE TRIGGER "room_info_size_insert" AFTER INSERT ON "room_info" BEGIN
    INSERT INTO "room_data_sizes" ("table_name", "room_id", "bytes")
        VALUES ('room_info', NEW."room_id", LENGTH(NEW."room_id") + LENGTH(NEW."data"))
        ON CONFLICT ("table_name", "room_id") DO UPDATE SET "bytes" = "bytes" + excluded."bytes";
END;

CREATE TRIGGER "room_info_size_delete" AFTER DELETE ON "room_info" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."data"))
        WHERE "table_name" = 'room_info' AND "room_id" = OLD."room_id";
END;

CREATE TRIGGER "room_info_size_update" AFTER UPDATE ON "room_info" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."data")) + (LENGTH(NEW."room_id") + LENGTH(NEW."data"))
        WHERE "table_name" = 'room_info' AND "room_id" = NEW."room_id";
END;

CREATE TRIGGER "state_event_size_insert" AFTER INSERT ON "state_event" BEGIN
    INSERT INTO "room_data_sizes" ("table_name", "room_id", "bytes")
        VALUES ('state_event', NEW."room_id", LENGTH(NEW."room_id") + LENGTH(NEW."event_type") + LENGTH(NEW."state_key") + LENGTH(NEW."data"))
        ON CONFLICT ("table_name", "room_id") DO UPDATE SET "bytes" = "bytes" + excluded."bytes";
END;

CREATE TRIGGER "state_event_size_delete" AFTER DELETE ON "state_event" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."event_type") + LENGTH(OLD."state_key") + LENGTH(OLD."data"))
        WHERE "table_name" = 'state_event' AND "room_id" = OLD."room_id";
END;

CREATE TRIGGER "state_event_size_update" AFTER UPDATE ON "state_event" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."event_type") + LENGTH(OLD."state_key") + LENGTH(OLD."data")) + (LENGTH(NEW."room_id") + LENGTH(NEW."event_type") + LENGTH(NEW."state_key") + LENGTH(NEW."data"))
        WHERE "table_name" = 'state_event' AND "room_id" = NEW."room_id";
END;

CREATE TRIGGER "member_size_insert" AFTER INSERT ON "member" BEGIN
    INSERT INTO "room_data_sizes" ("table_name", "room_id", "bytes")
        VALUES ('member', NEW."room_id", LENGTH(NEW."room_id") + LENGTH(NEW."user_id") + LENGTH(NEW."membership") + LENGTH(NEW."data"))
        ON CONFLICT ("table_name", "room_id") DO UPDATE SET "bytes" = "bytes" + excluded."bytes";
END;

CREATE TRIGGER "member_size_delete" AFTER DELETE ON "member" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."user_id") + LENGTH(OLD."membership") + LENGTH(OLD."data"))
        WHERE "table_name" = 'member' AND "room_id" = OLD."room_id";
END;

CREATE TRIGGER "member_size_update" AFTER UPDATE ON "member" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."user_id") + LENGTH(OLD."membership") + LENGTH(OLD."data")) + (LENGTH(NEW."room_id") + LENGTH(NEW."user_id") + LENGTH(NEW."membership") + LENGTH(NEW."data"))
        WHERE "table_name" = 'member' AND "room_id" = NEW."room_id";
END;

CREATE TRIGGER "profile_size_insert" AFTER INSERT ON "profile" BEGIN
    INSERT INTO "room_data_sizes" ("table_name", "room_id", "bytes")
        VALUES ('profile', NEW."room_id", LENGTH(NEW."room_id") + LENGTH(NEW."user_id") + LENGTH(NEW."data"))
        ON CONFLICT ("table_name", "room_id") DO UPDATE SET "bytes" = "bytes" + excluded."bytes";
END;

CREATE TRIGGER "profile_size_delete" AFTER DELETE ON "profile" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."user_id") + LENGTH(OLD."data"))
        WHERE "table_name" = 'profile' AND "room_id" = OLD."room_id";
END;

CREATE TRIGGER "profile_size_update" AFTER UPDATE ON "profile" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."user_id") + LENGTH(OLD."data")) + (LENGTH(NEW."room_id") + LENGTH(NEW."user_id") + LENGTH(NEW."data"))
        WHERE "table_name" = 'profile' AND "room_id" = NEW."room_id";
END;

CREATE TRIGGER "receipt_size_insert" AFTER INSERT ON "receipt" BEGIN
    INSERT INTO "room_data_sizes" ("table_name", "room_id", "bytes")
        VALUES ('receipt', NEW."room_id", LENGTH(NEW."room_id") + LENGTH(NEW."user_id") + LENGTH(NEW."receipt_type") + LENGTH(NEW."thread") + LENGTH(NEW."event_id") + LENGTH(NEW."data"))
        ON CONFLICT ("table_name", "room_id") DO UPDATE SET "bytes" = "bytes" + excluded."bytes";
END;

CREATE TRIGGER "receipt_size_delete" AFTER DELETE ON "receipt" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."user_id") + LENGTH(OLD."receipt_type") + LENGTH(OLD."thread") + LENGTH(OLD."event_id") + LENGTH(OLD."data"))
        WHERE "table_name" = 'receipt' AND "room_id" = OLD."room_id";
END;

CREATE TRIGGER "receipt_size_update" AFTER UPDATE ON "receipt" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."user_id") + LENGTH(OLD."receipt_type") + LENGTH(OLD."thread") + LENGTH(OLD."event_id") + LENGTH(OLD."data")) + (LENGTH(NEW."room_id") + LENGTH(NEW."user_id") + LENGTH(NEW."receipt_type") + LENGTH(NEW."thread") + LENGTH(NEW."event_id") + LENGTH(NEW."data"))
        WHERE "table_name" = 'receipt' AND "room_id" = NEW."room_id";
END;

CREATE TRIGGER "display_name_size_insert" AFTER INSERT ON "display_name" BEGIN
    INSERT INTO "room_data_sizes" ("table_name", "room_id", "bytes")
        VALUES ('display_name', NEW."room_id", LENGTH(NEW."room_id") + LENGTH(NEW."name") + LENGTH(NEW."data"))
        ON CONFLICT ("table_name", "room_id") DO UPDATE SET "bytes" = "bytes" + excluded."bytes";
END;

CREATE TRIGGER "display_name_size_delete" AFTER DELETE ON "display_name" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."name") + LENGTH(OLD."data"))
        WHERE "table_name" = 'display_name' AND "room_id" = OLD."room_id";
END;

CREATE TRIGGER "display_name_size_update" AFTER UPDATE ON "display_name" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."name") + LENGTH(OLD."data")) + (LENGTH(NEW."room_id") + LENGTH(NEW."name") + LENGTH(NEW."data"))
        WHERE "table_name" = 'display_name' AND "room_id" = NEW."room_id";
END;

CREATE TRIGGER "room_account_data_size_insert" AFTER INSERT ON "room_account_data" BEGIN
    INSERT INTO "room_data_sizes" ("table_name", "room_id", "bytes")
        VALUES ('room_account_data', NEW."room_id", LENGTH(NEW."room_id") + LENGTH(NEW."event_type") + LENGTH(NEW."data"))
        ON CONFLICT ("table_name", "room_id") DO UPDATE SET "bytes" = "bytes" + excluded."bytes";
END;

CREATE TRIGGER "room_account_data_size_delete" AFTER DELETE ON "room_account_data" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."event_type") + LENGTH(OLD."data"))
        WHERE "table_name" = 'room_account_data' AND "room_id" = OLD."room_id";
END;

CREATE TRIGGER "room_account_data_size_update" AFTER UPDATE ON "room_account_data" BEGIN
    UPDATE "room_data_sizes"
        SET "bytes" = "bytes" - (LENGTH(OLD."room_id") + LENGTH(OLD."event_type") + LENGTH(OLD."data")) + (LENGTH(NEW."room_id") + LENGTH(NEW."event_type") + LENGTH(NEW."data"))
        WHERE "table_name" = 'room_account_data' AND "room_id" = NEW."room_id";
END;
