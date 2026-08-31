The `TimelineItemContent::RtcNotification` now contains an optional `ActiveCallInfo` struct, which provides information
about the active call associated with the notification.
This allows to render in the timeline more detailed context about the active call (members, join state, etc...)