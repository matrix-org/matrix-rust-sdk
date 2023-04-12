// Copyright 2022 The Matrix.org Foundation C.I.C.
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

use std::sync::Arc;

use once_cell::sync::Lazy;
use ruma::{
    events::{room::message::MessageType, AnySyncTimelineEvent},
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedMxcUri, TransactionId, UserId,
};

use crate::Error;

mod content;
mod local;
mod remote;

pub use self::{
    content::{
        AnyOtherFullStateEventContent, BundledReactions, EncryptedMessage, InReplyToDetails,
        MemberProfileChange, MembershipChange, Message, OtherState, ReactionGroup, RepliedToEvent,
        RoomMembershipChange, Sticker, TimelineItemContent,
    },
    local::LocalEventTimelineItem,
    remote::RemoteEventTimelineItem,
};

/// An item in the timeline that represents at least one event.
///
/// There is always one main event that gives the `EventTimelineItem` its
/// identity but in many cases, additional events like reactions and edits are
/// also part of the item.
#[derive(Clone, Debug)]
pub struct EventTimelineItem {
    kind: EventTimelineItemKind,
}

#[derive(Clone, Debug)]
enum EventTimelineItemKind {
    /// A local event, not yet echoed back by the server.
    Local(LocalEventTimelineItem),
    /// An event received from the server.
    Remote(RemoteEventTimelineItem),
}

impl EventTimelineItem {
    /// Get the `LocalEventTimelineItem` if `self` is `Local`.
    pub fn as_local(&self) -> Option<&LocalEventTimelineItem> {
        match &self.kind {
            EventTimelineItemKind::Local(local_event_item) => Some(local_event_item),
            EventTimelineItemKind::Remote(_) => None,
        }
    }

    /// Get the `RemoteEventTimelineItem` if `self` is `Remote`.
    pub fn as_remote(&self) -> Option<&RemoteEventTimelineItem> {
        match &self.kind {
            EventTimelineItemKind::Local(_) => None,
            EventTimelineItemKind::Remote(remote_event_item) => Some(remote_event_item),
        }
    }

    pub(super) fn as_remote_mut(&mut self) -> Option<&mut RemoteEventTimelineItem> {
        match &mut self.kind {
            EventTimelineItemKind::Local(_) => None,
            EventTimelineItemKind::Remote(remote_event_item) => Some(remote_event_item),
        }
    }

    /// Get a unique identifier to identify the event item, either by using
    /// transaction ID or event ID in case of a local event, or by event ID in
    /// case of a remote event.
    pub fn unique_identifier(&self) -> String {
        match &self.kind {
            EventTimelineItemKind::Local(item) => match item.send_state() {
                EventSendState::Sent { event_id } => event_id.to_string(),
                _ => item.transaction_id().to_string(),
            },
            EventTimelineItemKind::Remote(item) => item.event_id().to_string(),
        }
    }

    /// Get the event's send state, if it is a local echo.
    pub fn send_state(&self) -> Option<&EventSendState> {
        match &self.kind {
            EventTimelineItemKind::Local(local) => Some(local.send_state()),
            EventTimelineItemKind::Remote(_) => None,
        }
    }

    /// Get the transaction ID of this item.
    ///
    /// The transaction ID is currently only kept until the remote echo for a
    /// local event is received.
    pub fn transaction_id(&self) -> Option<&TransactionId> {
        match &self.kind {
            EventTimelineItemKind::Local(local) => Some(local.transaction_id()),
            EventTimelineItemKind::Remote(_) => None,
        }
    }

    /// Get the event ID of this item.
    ///
    /// If this returns `Some(_)`, the event was successfully created by the
    /// server.
    ///
    /// Even if this is a local event, this can be `Some(_)` as the event ID can
    /// be known not just from the remote echo via `sync_events`, but also
    /// from the response of the send request that created the event.
    pub fn event_id(&self) -> Option<&EventId> {
        match &self.kind {
            EventTimelineItemKind::Local(local_event) => local_event.event_id(),
            EventTimelineItemKind::Remote(remote_event) => Some(remote_event.event_id()),
        }
    }

    /// Get the sender of this item.
    pub fn sender(&self) -> &UserId {
        match &self.kind {
            EventTimelineItemKind::Local(local_event) => local_event.sender(),
            EventTimelineItemKind::Remote(remote_event) => remote_event.sender(),
        }
    }

    /// Get the profile of the sender.
    pub fn sender_profile(&self) -> &TimelineDetails<Profile> {
        match &self.kind {
            EventTimelineItemKind::Local(local_event) => local_event.sender_profile(),
            EventTimelineItemKind::Remote(remote_event) => remote_event.sender_profile(),
        }
    }

    /// Get the content of this item.
    pub fn content(&self) -> &TimelineItemContent {
        match &self.kind {
            EventTimelineItemKind::Local(local_event) => local_event.content(),
            EventTimelineItemKind::Remote(remote_event) => remote_event.content(),
        }
    }

    /// Get the reactions of this item.
    pub fn reactions(&self) -> &BundledReactions {
        // There's not much of a point in allowing reactions to local echoes.
        static EMPTY_REACTIONS: Lazy<BundledReactions> = Lazy::new(Default::default);
        match &self.kind {
            EventTimelineItemKind::Local(_) => &EMPTY_REACTIONS,
            EventTimelineItemKind::Remote(remote_event) => remote_event.reactions(),
        }
    }

    /// Get the timestamp of this item.
    ///
    /// If this event hasn't been echoed back by the server yet, returns the
    /// time the local event was created. Otherwise, returns the origin
    /// server timestamp.
    pub fn timestamp(&self) -> MilliSecondsSinceUnixEpoch {
        match &self.kind {
            EventTimelineItemKind::Local(local_event) => local_event.timestamp(),
            EventTimelineItemKind::Remote(remote_event) => remote_event.timestamp(),
        }
    }

    /// Whether this timeline item was sent by the logged-in user themselves.
    pub fn is_own(&self) -> bool {
        match &self.kind {
            EventTimelineItemKind::Local(_) => true,
            EventTimelineItemKind::Remote(remote_event) => remote_event.is_own(),
        }
    }

    /// Flag indicating this timeline item can be edited by current user.
    pub fn is_editable(&self) -> bool {
        match self.content() {
            TimelineItemContent::Message(message) => {
                self.is_own()
                    && matches!(message.msgtype(), MessageType::Text(_) | MessageType::Emote(_))
            }
            _ => false,
        }
    }

    /// Get the raw JSON representation of the initial event (the one that
    /// caused this timeline item to be created).
    ///
    /// Returns `None` if this event hasn't been echoed back by the server
    /// yet.
    pub fn original_json(&self) -> Option<&Raw<AnySyncTimelineEvent>> {
        match &self.kind {
            EventTimelineItemKind::Local(_local_event) => None,
            EventTimelineItemKind::Remote(remote_event) => Some(remote_event.original_json()),
        }
    }

    /// Get the raw JSON representation of the latest edit, if any.
    pub fn latest_edit_json(&self) -> Option<&Raw<AnySyncTimelineEvent>> {
        match &self.kind {
            EventTimelineItemKind::Local(_local_event) => None,
            EventTimelineItemKind::Remote(remote_event) => remote_event.latest_edit_json(),
        }
    }

    /// Clone the current event item, and apply an edit to it.
    pub(super) fn apply_edit(
        &self,
        new_content: TimelineItemContent,
        edit_json: Option<Raw<AnySyncTimelineEvent>>,
    ) -> Self {
        let kind = match &self.kind {
            EventTimelineItemKind::Local(local_event) => {
                EventTimelineItemKind::Local(local_event.with_content(new_content))
            }
            EventTimelineItemKind::Remote(remote_event) => {
                EventTimelineItemKind::Remote(remote_event.apply_edit(new_content, edit_json))
            }
        };

        Self { kind }
    }

    /// Clone the current event item, and update its `sender_profile`.
    pub(super) fn with_sender_profile(&self, sender_profile: TimelineDetails<Profile>) -> Self {
        let kind = match &self.kind {
            EventTimelineItemKind::Local(local_event) => {
                EventTimelineItemKind::Local(local_event.with_sender_profile(sender_profile))
            }
            EventTimelineItemKind::Remote(remote_event) => {
                EventTimelineItemKind::Remote(remote_event.with_sender_profile(sender_profile))
            }
        };

        Self { kind }
    }
}

/// This type represents the "send state" of a local event timeline item.
#[derive(Clone, Debug)]
pub enum EventSendState {
    /// The local event has not been sent yet.
    NotSentYet,
    /// The local event has been sent to the server, but unsuccessfully: The
    /// sending has failed.
    SendingFailed {
        /// Details about how sending the event failed.
        error: Arc<Error>,
    },
    /// The local event has been sent successfully to the server.
    Sent {
        /// The event ID assigned by the server.
        event_id: OwnedEventId,
    },
}

impl From<LocalEventTimelineItem> for EventTimelineItem {
    fn from(value: LocalEventTimelineItem) -> Self {
        Self { kind: EventTimelineItemKind::Local(value) }
    }
}

impl From<RemoteEventTimelineItem> for EventTimelineItem {
    fn from(value: RemoteEventTimelineItem) -> Self {
        Self { kind: EventTimelineItemKind::Remote(value) }
    }
}

/// The display name and avatar URL of a room member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// The display name, if set.
    pub display_name: Option<String>,
    /// Whether the display name is ambiguous.
    ///
    /// Note that in rooms with lazy-loading enabled, this could be `false` even
    /// though the display name is actually ambiguous if not all member events
    /// have been seen yet.
    pub display_name_ambiguous: bool,
    /// The avatar URL, if set.
    pub avatar_url: Option<OwnedMxcUri>,
}

/// Some details of an [`EventTimelineItem`] that may require server requests
/// other than just the regular
/// [`sync_events`][ruma::api::client::sync::sync_events].
#[derive(Clone, Debug)]
pub enum TimelineDetails<T> {
    /// The details are not available yet, and have not been request from the
    /// server.
    Unavailable,

    /// The details are not available yet, but have been requested.
    Pending,

    /// The details are available.
    Ready(T),

    /// An error occurred when fetching the details.
    Error(Arc<Error>),
}

impl<T> TimelineDetails<T> {
    pub(crate) fn from_initial_value(value: Option<T>) -> Self {
        match value {
            Some(v) => Self::Ready(v),
            None => Self::Unavailable,
        }
    }

    pub(crate) fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable)
    }

    pub(crate) fn contains<U>(&self, value: &U) -> bool
    where
        T: PartialEq<U>,
    {
        matches!(self, Self::Ready(v) if v == value)
    }
}
