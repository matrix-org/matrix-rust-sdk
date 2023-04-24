//! Types related to events.

use napi::bindgen_prelude::{FromNapiValue, ToNapiValue};
use napi_derive::*;
use ruma::events::room::history_visibility::HistoryVisibility as RumaHistoryVisibility;

/// Who can see a room's history.
#[napi]
#[derive(Debug)]
pub enum HistoryVisibility {
    /// Previous events are accessible to newly joined members from
    /// the point they were invited onwards.
    ///
    /// Events stop being accessible when the member's state changes
    /// to something other than *invite* or *join*.
    Invited,

    /// Previous events are accessible to newly joined members from
    /// the point they joined the room onwards.
    ///
    /// Events stop being accessible when the member's state changes
    /// to something other than *join*.
    Joined,

    /// Previous events are always accessible to newly joined members.
    ///
    /// All events in the room are accessible, even those sent when
    /// the member was not a part of the room.
    Shared,

    /// All events while this is the `HistoryVisibility` value may be
    /// shared by any participating homeserver with anyone, regardless
    /// of whether they have ever joined the room.
    WorldReadable,
}

impl From<HistoryVisibility> for RumaHistoryVisibility {
    fn from(value: HistoryVisibility) -> Self {
        use HistoryVisibility::*;

        match value {
            Invited => Self::Invited,
            Joined => Self::Joined,
            Shared => Self::Shared,
            WorldReadable => Self::WorldReadable,
        }
    }
}

impl From<RumaHistoryVisibility> for HistoryVisibility {
    fn from(value: RumaHistoryVisibility) -> Self {
        use RumaHistoryVisibility::*;

        match value {
            Invited => Self::Invited,
            Joined => Self::Joined,
            Shared => Self::Shared,
            WorldReadable => Self::WorldReadable,
            _ => unreachable!("Unknown variant"),
        }
    }
}
