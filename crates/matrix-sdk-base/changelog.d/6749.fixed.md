The `Client::forget_room` contained two bugs:

1. It was removing events for the `RoomEventCache` associated to the given
   `room_id` only, missing the `ThreadEventCache`s and the `PinnedEventsCache`.
2. It was removing in-store data only, missing the in-memory data!

The patch adds an `EventCache::forget_room` methods, called by
`Client::forget_room`, to properly clears all events.
