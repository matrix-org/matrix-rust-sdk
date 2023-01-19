-- TODO: merge the two first tables
CREATE TABLE "metadata" (
    "key" TEXT PRIMARY KEY NOT NULL,
    "value" BLOB NOT NULL
);

CREATE TABLE "account_data" (
    "key" TEXT PRIMARY KEY NOT NULL,
    "value" BLOB NOT NULL
);

CREATE TABLE "session" (
    "session_id" BLOB PRIMARY KEY NOT NULL,
    "sender_key" BLOB NOT NULL,
    "session_data" BLOB NOT NULL
);
CREATE INDEX "session_sender_key_idx"
    ON "session" ("sender_key");

CREATE TABLE "inbound_group_session" (
    "session_id" BLOB PRIMARY KEY NOT NULL,
    "room_id" BLOB NOT NULL,
    "session_data" BLOB NOT NULL
);
CREATE INDEX "inbound_group_session_room_id_idx"
    ON "inbound_group_session" ("room_id");

CREATE TABLE "outbound_group_session" (
    "room_id" BLOB PRIMARY KEY NOT NULL,
    "session_data" BLOB NOT NULL
);
