The cached user-defined notification mode of a room, returned by
`Room::cached_user_defined_notification_mode()`, is now persisted in the state
store instead of only being kept in memory. Previously, a muted room was read
back as having no user-defined notification mode after an app restart, until
the next sync recomputed it. The cache is now also filled by the `/sync` (v2)
loop, not only by sliding sync, and `Room::user_defined_notification_mode()`
now clears a stale cached value when the room-specific push rule has been
removed.
