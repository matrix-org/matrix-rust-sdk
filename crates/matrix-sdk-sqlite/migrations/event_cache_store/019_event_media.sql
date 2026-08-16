-- The `mxc://` URIs referenced by each stored media message (images, videos,
-- audios, files, galleries), so that the media contents can be attributed to
-- rooms without decoding the events. The URI is an encrypted value.
CREATE TABLE "event_media" (
    "room_id" BLOB NOT NULL,
    "event_id" BLOB NOT NULL,
    "uri" BLOB NOT NULL,

    FOREIGN KEY ("event_id") REFERENCES "events"("event_id") ON DELETE CASCADE
);

CREATE INDEX "event_media_room_id_idx" ON "event_media" ("room_id");
CREATE INDEX "event_media_event_id_idx" ON "event_media" ("event_id");

-- Whether an event's media URIs are in `event_media`; rows predating the table
-- are scanned lazily.
ALTER TABLE "events" ADD COLUMN "media_indexed" INTEGER NOT NULL DEFAULT 0;
