An unrecoverable send error no longer disables the room's send queue. The failed
request is wedged, which already blocks the room's queue while preserving ordering,
so unwedging or aborting it is now enough to get the queue going again;
previously the client also had to call `RoomSendQueue::set_enabled(true)`.
