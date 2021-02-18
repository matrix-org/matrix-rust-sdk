use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::identifiers::{EventId, RoomId, UserId};

mod enums;
pub mod events;
mod members;
mod sync;

pub use enums::*;
pub use members::*;
pub use sync::*;

/// A change in ambiguity of room members that an `m.room.member` event
/// triggers.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AmbiguityChange {
    /// Is the member that is contained in the state key of the `m.room.member`
    /// event itself ambiguous because of the event.
    pub member_ambiguous: bool,
    /// Has another user been disambiguated because of this event.
    pub disambiguated_member: Option<UserId>,
    /// Has another user become ambiguous because of this event.
    pub ambiguated_member: Option<UserId>,
}

/// Collection of ambiguioty changes that room member events trigger.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AmbiguityChanges {
    /// A map from room id to a map of an event id to the `AmbiguityChange` that
    /// the event with the given id caused.
    pub changes: BTreeMap<RoomId, BTreeMap<EventId, AmbiguityChange>>,
}
