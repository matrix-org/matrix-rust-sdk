Add `RoomInfo::cached_user_defined_notification_mode()` and
`RoomInfo::set_cached_user_defined_notification_mode()`, to read and update the
cached user-defined notification mode directly on a `RoomInfo`, for example in
the closure given to `Room::update_and_save_room_info()`, so that the value is
persisted in the state store.
