CREATE TABLE "secrets_inbox" (
    "secret_name" BLOB NOT NULL,
    "secret" BLOB NOT NULL
);
CREATE INDEX "secrets_inbox_secret_name_idx"
    ON "secrets_inbox" ("secret_name", "secret");
