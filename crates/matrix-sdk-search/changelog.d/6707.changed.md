Performance optimisation: Cache the Tantivy `IndexReader` on `RoomIndex` because
creating it is costly.
