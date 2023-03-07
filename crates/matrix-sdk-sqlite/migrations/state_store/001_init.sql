-- basic kv data like the database version and store cipher
CREATE TABLE "kv" (
    "key" TEXT PRIMARY KEY NOT NULL,
    "value" BLOB NOT NULL
);

-- another general-purpose kv table, but with blob keys
-- used for sync token, filters and other things
--
-- in theory, we could use one table for both since sqlite has dynamic typing
-- (column types are basically meaningless), but this is a cleaner solution
CREATE TABLE "kv_blob" (
    "key" BLOB PRIMARY KEY NOT NULL,
    "value" BLOB NOT NULL
);

CREATE TABLE "room_info" (
    "room_id" BLOB PRIMARY KEY NOT NULL,
    "stripped" BOOLEAN NOT NULL,
    "data" BLOB NOT NULL
);

CREATE TABLE "state_event" (
    "room_id" BLOB NOT NULL,
    "event_type" BLOB NOT NULL,
    "state_key" BLOB NOT NULL,
    "stripped" BOOLEAN NOT NULL,
    "data" BLOB NOT NULL,

    PRIMARY KEY ("room_id", "event_type", "state_key")
);
CREATE INDEX "state_event_room_id_event_type"
    ON "state_event" ("room_id", "event_type");

CREATE TABLE "profile" (
    "room_id" BLOB NOT NULL,
    "user_id" BLOB NOT NULL,
    "data" BLOB NOT NULL,

    PRIMARY KEY ("room_id", "user_id")
);

CREATE TABLE "global_account_data" (
    "event_type" BLOB PRIMARY KEY NOT NULL,
    "data" BLOB NOT NULL
);

CREATE TABLE "room_account_data" (
    "room_id" BLOB NOT NULL,
    "event_type" BLOB NOT NULL,
    "data" BLOB NOT NULL,

    PRIMARY KEY ("room_id", "event_type")
);
