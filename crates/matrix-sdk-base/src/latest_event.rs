//! Utilities for working with events to decide whether they are suitable for
//! use as a [crate::Room::latest_event].

use matrix_sdk_common::deserialized_responses::TimelineEvent;
use ruma::MilliSecondsSinceUnixEpoch;
use serde::{Deserialize, Serialize};

use crate::store::SerializableEventContent;

/// A latest event value!
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub enum LatestEventValue {
    /// No value has been computed yet, or no candidate value was found.
    #[default]
    None,

    /// The latest event represents a remote event.
    Remote(RemoteLatestEventValue),

    /// The latest event represents a local event that is sending.
    LocalIsSending(LocalLatestEventValue),

    /// The latest event represents a local event that cannot be sent, either
    /// because a previous local event, or this local event cannot be sent.
    LocalCannotBeSent(LocalLatestEventValue),
}

/// Represents the value for [`LatestEventValue::Remote`].
pub type RemoteLatestEventValue = TimelineEvent;

/// Represents the value for [`LatestEventValue::LocalIsSending`] and
/// [`LatestEventValue::LocalCannotBeSent`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalLatestEventValue {
    /// The time where the event has been created (by this module).
    pub timestamp: MilliSecondsSinceUnixEpoch,

    /// The content of the local event.
    pub content: SerializableEventContent,
}
