-- Journal of the rooms whose linked chunks were modified, tagged with the
-- cross-process lock generation under which the write happened. A process
-- recovering from a dirtied lock reads the rooms touched since the generation
-- it last held, and reloads only those instead of every room it has in memory.

CREATE TABLE "linked_chunk_touches" (
    -- Hashed room ID (deterministic key encoding), deduplicating rows within a
    -- generation. The single byte '*' marks a store-wide operation: recovery
    -- must then assume everything changed.
    "hashed_room_id" BLOB NOT NULL,
    -- The room ID, value-encoded so it can be decoded back. Empty for the
    -- store-wide marker.
    "room_id" BLOB NOT NULL,
    -- The cross-process lock generation under which the write happened.
    "generation" INTEGER NOT NULL,
    PRIMARY KEY ("hashed_room_id", "generation")
) WITHOUT ROWID;

CREATE INDEX "linked_chunk_touches_generation" ON "linked_chunk_touches" ("generation");
