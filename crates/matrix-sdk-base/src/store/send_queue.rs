// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! All data types related to the send queue.

use std::{collections::BTreeMap, fmt, ops::Deref};

use as_variant::as_variant;
use ruma::{
    events::{AnyMessageLikeEventContent, EventContent as _, RawExt as _},
    serde::Raw,
    OwnedDeviceId, OwnedEventId, OwnedTransactionId, OwnedUserId, TransactionId,
};
use serde::{Deserialize, Serialize};

/// A thin wrapper to serialize a `AnyMessageLikeEventContent`.
#[derive(Clone, Serialize, Deserialize)]
pub struct SerializableEventContent {
    event: Raw<AnyMessageLikeEventContent>,
    event_type: String,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SerializableEventContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Don't include the event in the debug display.
        f.debug_struct("SerializedEventContent")
            .field("event_type", &self.event_type)
            .finish_non_exhaustive()
    }
}

impl SerializableEventContent {
    /// Create a [`SerializableEventContent`] from a raw
    /// [`AnyMessageLikeEventContent`] along with its type.
    pub fn from_raw(event: Raw<AnyMessageLikeEventContent>, event_type: String) -> Self {
        Self { event_type, event }
    }

    /// Create a [`SerializableEventContent`] from an
    /// [`AnyMessageLikeEventContent`].
    pub fn new(event: &AnyMessageLikeEventContent) -> Result<Self, serde_json::Error> {
        Ok(Self::from_raw(Raw::new(event)?, event.event_type().to_string()))
    }

    /// Convert a [`SerializableEventContent`] back into a
    /// [`AnyMessageLikeEventContent`].
    pub fn deserialize(&self) -> Result<AnyMessageLikeEventContent, serde_json::Error> {
        self.event.deserialize_with_type(self.event_type.clone().into())
    }

    /// Returns the raw event content along with its type.
    ///
    /// Useful for callers manipulating custom events.
    pub fn raw(&self) -> (&Raw<AnyMessageLikeEventContent>, &str) {
        (&self.event, &self.event_type)
    }
}

/// The kind of a send queue request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum QueuedRequestKind {
    /// An event to be sent via the send queue.
    Event {
        /// The content of the message-like event we'd like to send.
        content: SerializableEventContent,
    },
}

/// A request to be sent with a send queue.
#[derive(Clone)]
pub struct QueuedEvent {
    /// The kind of queued request we're going to send.
    pub kind: QueuedRequestKind,

    /// Unique transaction id for the queued request, acting as a key.
    pub transaction_id: OwnedTransactionId,

    /// Error returned when the request couldn't be sent and is stuck in the
    /// unrecoverable state.
    ///
    /// `None` if the request is in the queue, waiting to be sent.
    pub error: Option<QueueWedgeError>,
}

impl QueuedEvent {
    /// Returns `Some` if the queued request is about sending an event.
    pub fn as_event(&self) -> Option<&SerializableEventContent> {
        as_variant!(&self.kind, QueuedRequestKind::Event { content } => content)
    }

    /// True if the event couldn't be sent because of an unrecoverable API
    /// error. See [`Self::error`] for more details on the reason.
    pub fn is_wedged(&self) -> bool {
        self.error.is_some()
    }
}

/// Represents a failed to send unrecoverable error of an event sent via the
/// send queue.
///
/// It is a serializable representation of a client error, see
/// `From` implementation for more details. These errors can not be
/// automatically retried, but yet some manual action can be taken before retry
/// sending. If not the only solution is to delete the local event.
#[derive(Clone, Debug, Serialize, Deserialize, thiserror::Error)]
pub enum QueueWedgeError {
    /// This error occurs when there are some insecure devices in the room, and
    /// the current encryption setting prohibits sharing with them.
    #[error("There are insecure devices in the room")]
    InsecureDevices {
        /// The insecure devices as a Map of userID to deviceID.
        user_device_map: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>>,
    },

    /// This error occurs when a previously verified user is not anymore, and
    /// the current encryption setting prohibits sharing when it happens.
    #[error("Some users that were previously verified are not anymore")]
    IdentityViolations {
        /// The users that are expected to be verified but are not.
        users: Vec<OwnedUserId>,
    },

    /// It is required to set up cross-signing and properly verify the current
    /// session before sending.
    #[error("Own verification is required")]
    CrossVerificationRequired,

    /// Other errors.
    #[error("Other unrecoverable error: {msg}")]
    GenericApiError {
        /// Description of the error.
        msg: String,
    },
}

/// The specific user intent that characterizes a [`DependentQueuedEvent`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DependentQueuedEventKind {
    /// The event should be edited.
    Edit {
        /// The new event for the content.
        new_content: SerializableEventContent,
    },

    /// The event should be redacted/aborted/removed.
    Redact,

    /// The event should be reacted to, with the given key.
    React {
        /// Key used for the reaction.
        key: String,
    },
}

/// A transaction id identifying a [`DependentQueuedEvent`] rather than its
/// parent [`QueuedEvent`].
///
/// This thin wrapper adds some safety to some APIs, making it possible to
/// distinguish between the parent's `TransactionId` and the dependent event's
/// own `TransactionId`.
#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChildTransactionId(OwnedTransactionId);

impl ChildTransactionId {
    /// Returns a new [`ChildTransactionId`].
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(TransactionId::new())
    }
}

impl Deref for ChildTransactionId {
    type Target = TransactionId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<String> for ChildTransactionId {
    fn from(val: String) -> Self {
        Self(val.into())
    }
}

impl From<ChildTransactionId> for OwnedTransactionId {
    fn from(val: ChildTransactionId) -> Self {
        val.0
    }
}

/// An event to be sent, depending on a [`QueuedEvent`] to be sent first.
///
/// Depending on whether the event has been sent or not, this will either update
/// the local echo in the storage, or send an event equivalent to the user
/// intent to the homeserver.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DependentQueuedEvent {
    /// Unique identifier for this dependent queued event.
    ///
    /// Useful for deletion.
    pub own_transaction_id: ChildTransactionId,

    /// The kind of user intent.
    pub kind: DependentQueuedEventKind,

    /// Transaction id for the parent's local echo / used in the server request.
    ///
    /// Note: this is the transaction id used for the depended-on event, i.e.
    /// the one that was originally sent and that's being modified with this
    /// dependent event.
    pub parent_transaction_id: OwnedTransactionId,

    /// If the parent event has been sent, the parent's event identifier
    /// returned by the server once the local echo has been sent out.
    ///
    /// Note: this is the event id used for the depended-on event after it's
    /// been sent, not for a possible event that could have been sent
    /// because of this [`DependentQueuedEvent`].
    pub event_id: Option<OwnedEventId>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for QueuedEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Hide the content from the debug log.
        f.debug_struct("QueuedEvent")
            .field("transaction_id", &self.transaction_id)
            .field("is_wedged", &self.is_wedged())
            .finish_non_exhaustive()
    }
}
