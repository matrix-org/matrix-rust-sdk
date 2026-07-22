The `EventCacheStore::remove_room` method has been removed: it is replaced
by `EventCacheStore::clear_all_events` which gains a new argument:
`Option<&RoomId>`.

Before:

```rust
event_cache_store.remove_room(room_id).await?;
```

After:

```rust
event_cache_store.clear_all_events(Some(room_id)).await?;
```

Note that `clear_all_events(None)` will remove events for all the rooms. By
room, one must understand all events for a particular room, which includes all
possible caches, like `RoomEventCache`, `ThreadEventCache` and
`PinnedEventsCache`.
