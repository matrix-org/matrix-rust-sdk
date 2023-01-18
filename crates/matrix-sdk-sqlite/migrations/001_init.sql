CREATE TABLE "metadata" (
    "key" TEXT PRIMARY KEY NOT NULL,
    "value" BLOB NOT NULL
);

CREATE TABLE "account_data" (
    "key" TEXT PRIMARY KEY NOT NULL,
    "value" BLOB NOT NULL
);

CREATE TABLE "session" (
    "session_id" TEXT PRIMARY KEY NOT NULL,
    "sender_key" BLOB NOT NULL,
    "session_data" BLOB NOT NULL
);

CREATE INDEX "session_sender_key_idx"
    ON "session" ("sender_key");
