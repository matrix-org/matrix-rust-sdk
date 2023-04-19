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

use indexmap::IndexMap;
use matrix_sdk_base::deserialized_responses::EncryptionInfo;
use once_cell::sync::Lazy;
use ruma::{
    events::{receipt::Receipt, room::message::MessageType, AnySyncTimelineEvent},
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedMxcUri, OwnedUserId, TransactionId,
    UserId,
};

use crate::Error;

mod content;
mod local;
mod remote;

pub use self::content::{
    AnyOtherFullStateEventContent, BundledReactions, EncryptedMessage, InReplyToDetails,
    MemberProfileChange, MembershipChange, Message, OtherState, ReactionGroup, RepliedToEvent,
    RoomMembershipChange, Sticker, TimelineItemContent,
};
pub(super) use self::{local::LocalEventTimelineItem, remote::RemoteEventTimelineItem};

/// An item in the timeline that represents at least one event.
///
/// There is always one main event that gives the `EventTimelineItem` its
/// identity but in many cases, additional events like reactions and edits are
/// also part of the item.
#[derive(Clone, Debug)]
pub struct EventTimelineItem {
    /// The sender of the event.
    pub(super) sender: OwnedUserId,
    /// The sender's profile of the event.
    pub(super) sender_profile: TimelineDetails<Profile>,
    /// The timestamp of the event.
    pub(super) timestamp: MilliSecondsSinceUnixEpoch,
    /// The content of the event.
    pub(super) content: TimelineItemContent,
    /// The kind of event timeline item, local or remote.
    pub(super) kind: EventTimelineItemKind,
}

#[derive(Clone, Debug)]
pub(super) enum EventTimelineItemKind {
    /// A local event, not yet echoed back by the server.
    Local(LocalEventTimelineItem),
    /// An event received from the server.
    Remote(RemoteEventTimelineItem),
}

impl EventTimelineItem {
    pub(super) fn new(
        sender: OwnedUserId,
        sender_profile: TimelineDetails<Profile>,
        timestamp: MilliSecondsSinceUnixEpoch,
        content: TimelineItemContent,
        kind: EventTimelineItemKind,
    ) -> Self {
        Self { sender, sender_profile, timestamp, content, kind }
    }

    /// Check whether this item is a local echo.
    ///
    /// This returns `true` for events created locally, until the server echoes
    /// back the full event as part of a sync response.
    pub fn is_local_echo(&self) -> bool {
        matches!(self.kind, EventTimelineItemKind::Local(_))
    }

    /// Get the `LocalEventTimelineItem` if `self` is `Local`.
    pub(super) fn as_local(&self) -> Option<&LocalEventTimelineItem> {
        match &self.kind {
            EventTimelineItemKind::Local(local_event_item) => Some(local_event_item),
            EventTimelineItemKind::Remote(_) => None,
        }
    }

    /// Get the `RemoteEventTimelineItem` if `self` is `Remote`.
    pub(super) fn as_remote(&self) -> Option<&RemoteEventTimelineItem> {
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
            EventTimelineItemKind::Local(item) => match &item.send_state {
                EventSendState::Sent { event_id } => event_id.to_string(),
                _ => item.transaction_id.to_string(),
            },
            EventTimelineItemKind::Remote(item) => item.event_id.to_string(),
        }
    }

    /// Get the event's send state, if it is a local echo.
    pub fn send_state(&self) -> Option<&EventSendState> {
        match &self.kind {
            EventTimelineItemKind::Local(local) => Some(&local.send_state),
            EventTimelineItemKind::Remote(_) => None,
        }
    }

    /// Get the transaction ID of this item.
    ///
    /// The transaction ID is currently only kept until the remote echo for a
    /// local event is received.
    pub fn transaction_id(&self) -> Option<&TransactionId> {
        match &self.kind {
            EventTimelineItemKind::Local(local) => Some(&local.transaction_id),
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
            EventTimelineItemKind::Remote(remote_event) => Some(&remote_event.event_id),
        }
    }

    /// Get the sender of this item.
    pub fn sender(&self) -> &UserId {
        &self.sender
    }

    /// Get the profile of the sender.
    pub fn sender_profile(&self) -> &TimelineDetails<Profile> {
        &self.sender_profile
    }

    /// Get the content of this item.
    pub fn content(&self) -> &TimelineItemContent {
        &self.content
    }

    /// Get the reactions of this item.
    pub fn reactions(&self) -> &BundledReactions {
        // There's not much of a point in allowing reactions to local echoes.
        static EMPTY_REACTIONS: Lazy<BundledReactions> = Lazy::new(Default::default);
        match &self.kind {
            EventTimelineItemKind::Local(_) => &EMPTY_REACTIONS,
            EventTimelineItemKind::Remote(remote_event) => &remote_event.reactions,
        }
    }

    /// Get the read receipts of this item.
    ///
    /// The key is the ID of a room member and the value are details about the
    /// read receipt.
    ///
    /// Note that currently this ignores threads.
    pub fn read_receipts(&self) -> &IndexMap<OwnedUserId, Receipt> {
        static EMPTY_RECEIPTS: Lazy<IndexMap<OwnedUserId, Receipt>> = Lazy::new(Default::default);
        match &self.kind {
            EventTimelineItemKind::Local(_) => &EMPTY_RECEIPTS,
            EventTimelineItemKind::Remote(remote_event) => &remote_event.read_receipts,
        }
    }

    /// Get the timestamp of this item.
    ///
    /// If this event hasn't been echoed back by the server yet, returns the
    /// time the local event was created. Otherwise, returns the origin
    /// server timestamp.
    pub fn timestamp(&self) -> MilliSecondsSinceUnixEpoch {
        self.timestamp
    }

    /// Whether this timeline item was sent by the logged-in user themselves.
    pub fn is_own(&self) -> bool {
        match &self.kind {
            EventTimelineItemKind::Local(_) => true,
            EventTimelineItemKind::Remote(remote_event) => remote_event.is_own,
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

    /// Whether the event should be highlighted in the timeline.
    pub fn is_highlighted(&self) -> bool {
        match &self.kind {
            EventTimelineItemKind::Local(_) => false,
            EventTimelineItemKind::Remote(remote_event) => remote_event.is_highlighted,
        }
    }

    /// Get the encryption information for the event, if any.
    pub fn encryption_info(&self) -> Option<&EncryptionInfo> {
        match &self.kind {
            EventTimelineItemKind::Local(_) => None,
            EventTimelineItemKind::Remote(remote_event) => remote_event.encryption_info.as_ref(),
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
            EventTimelineItemKind::Remote(remote_event) => Some(&remote_event.original_json),
        }
    }

    /// Get the raw JSON representation of the latest edit, if any.
    pub fn latest_edit_json(&self) -> Option<&Raw<AnySyncTimelineEvent>> {
        match &self.kind {
            EventTimelineItemKind::Local(_local_event) => None,
            EventTimelineItemKind::Remote(remote_event) => remote_event.latest_edit_json.as_ref(),
        }
    }

    pub(super) fn set_content(&mut self, content: TimelineItemContent) {
        self.content = content;
    }

    /// Clone the current event item, and update its `kind`.
    pub(super) fn with_kind(&self, kind: impl Into<EventTimelineItemKind>) -> Self {
        Self { kind: kind.into(), ..self.clone() }
    }

    /// Clone the current event item, and apply an edit to it.
    pub(super) fn apply_edit(
        &self,
        new_content: TimelineItemContent,
        edit_json: Option<Raw<AnySyncTimelineEvent>>,
    ) -> Self {
        let mut new = self.clone();
        new.content = new_content;
        if let EventTimelineItemKind::Remote(r) = &mut new.kind {
            r.latest_edit_json = edit_json;
        }

        new
    }

    /// Clone the current event item, and update its `sender_profile`.
    pub(super) fn with_sender_profile(&self, sender_profile: TimelineDetails<Profile>) -> Self {
        Self { sender_profile, ..self.clone() }
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

impl From<LocalEventTimelineItem> for EventTimelineItemKind {
    fn from(value: LocalEventTimelineItem) -> Self {
        EventTimelineItemKind::Local(value)
    }
}

impl From<RemoteEventTimelineItem> for EventTimelineItemKind {
    fn from(value: RemoteEventTimelineItem) -> Self {
        EventTimelineItemKind::Remote(value)
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
