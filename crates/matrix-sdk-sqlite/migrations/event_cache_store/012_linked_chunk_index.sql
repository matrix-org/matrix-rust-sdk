-- Add an index on `linked_chunks.linked_chunk_id` to speed up the `load_last_chunk` query.
CREATE INDEX "linked_chunk_id_index"
    ON "linked_chunks" ("linked_chunk_id");

CREATE INDEX "linked_chunk_id_and_next_index"
    ON "linked_chunks" ("linked_chunk_id", "next");
