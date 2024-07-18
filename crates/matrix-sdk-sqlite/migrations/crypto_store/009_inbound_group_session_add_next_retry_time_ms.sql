ALTER TABLE "inbound_group_session"
    ADD COLUMN "next_retry_time_ms" INTEGER;

CREATE INDEX "inbound_group_session_next_retry_time_ms_idx"
    ON "inbound_group_session" ("next_retry_time_ms");
