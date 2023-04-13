use std::fmt;

use ruma::{serde::Raw, OwnedEventId};

/// A wrapper around `Raw` that implements `Debug` in a way that only prints the
/// event ID and event type.
pub struct DebugRawEvent<'a, T>(pub &'a Raw<T>);

#[cfg(not(tarpaulin_include))]
impl<T> fmt::Debug for DebugRawEvent<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawEvent")
            .field("event_id", &DebugEventId(self.0.get_field("event_id")))
            .field("event_type", &DebugEventType(self.0.get_field("event_type")))
            .finish_non_exhaustive()
    }
}

/// A wrapper around `Raw` that implements `Debug` in a way that only prints the
/// event type.
pub struct DebugRawEventNoId<'a, T>(pub &'a Raw<T>);

#[cfg(not(tarpaulin_include))]
impl<T> fmt::Debug for DebugRawEventNoId<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawEvent")
            .field("event_type", &DebugEventType(self.0.get_field("event_type")))
            .finish_non_exhaustive()
    }
}

struct DebugEventId(serde_json::Result<Option<OwnedEventId>>);

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for DebugEventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Ok(Some(id)) => id.fmt(f),
            Ok(None) => f.write_str("Missing"),
            Err(e) => f.debug_tuple("Invalid").field(&e).finish(),
        }
    }
}

struct DebugEventType(serde_json::Result<Option<String>>);

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for DebugEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Ok(Some(id)) => id.fmt(f),
            Ok(None) => f.write_str("Missing"),
            Err(e) => f.debug_tuple("Invalid").field(&e).finish(),
        }
    }
}
