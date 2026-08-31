The `TimelineEvent::event_id` now returns an `Option<&EventId>` instead of an
`Option<OwnedEventId>`. This is possible because the event ID is now eagerly
parsed when constructing this type and kept in memory for performance concerns.

This cached event ID is not serialized: it is backward compatible regarding the
storage, while it is not regarding the `event_id()` method signature.

If one needs an `OwnedEventId` from an `&EventId`, let's just use
`EventId::to_owned`.
