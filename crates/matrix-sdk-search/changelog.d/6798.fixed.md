Use `ReloadPolicy::Manual` for the Tantivy `IndexReader` of a `RoomIndex`.
Tantivy's default policy spawns a meta file watcher thread per index, i.e. one
per room, and panics if the thread cannot be spawned. Commits already reload the
reader explicitly, so the watcher was pure overhead.
