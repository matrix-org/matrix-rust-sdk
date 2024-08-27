ALTER TABLE "inbound_group_session"
    ADD COLUMN "sender_key" BLOB;

ALTER TABLE "inbound_group_session"
    ADD COLUMN "sender_data_type" INTEGER;

CREATE INDEX "inbound_group_session_curve_key_sender_data_type_idx"
    ON "inbound_group_session" ("sender_key", "sender_data_type");
