Add `RoomListService::remove_room_subscriptions`, which removes the
subscriptions of the given rooms, and
`RoomListService::reset_and_add_room_subscriptions`, which replaces the whole
subscription set and marks the members of the new rooms as missing. Both forward
to the room subscription methods of `SlidingSync` with the room list settings.
