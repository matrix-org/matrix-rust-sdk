The `OrderTracker` inside the Event Cache no longer gets out of sync, removing a
panic. It was getting out of sync when the `RoomEventCache` or the
`ThreadEventCache` was reloaded because the Event Cache cross-process lock was
dirty. The caches were reloaded but not the `OrderTracker` which needs the
`LinkedChunk` metadata to be re-initialised.
