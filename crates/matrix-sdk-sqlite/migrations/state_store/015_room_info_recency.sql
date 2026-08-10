-- The room's recency stamp (`bump_stamp` of MSC4186), stored in the clear so
-- rooms can be loaded most-recent first. Only an ordering leaks here: the
-- write order of the rows (rowid) already reveals roughly the same thing.
ALTER TABLE "room_info" ADD COLUMN "recency" INTEGER NOT NULL DEFAULT 0;

CREATE INDEX "room_info_recency" ON "room_info" ("recency");
