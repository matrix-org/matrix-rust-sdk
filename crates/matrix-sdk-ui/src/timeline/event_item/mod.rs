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

use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
};

use as_variant::as_variant;
use indexmap::IndexMap;
use matrix_sdk::{
    deserialized_responses::{EncryptionInfo, ShieldState},
    send_queue::{SendHandle, SendReactionHandle},
    Client, Error,
};
use matrix_sdk_base::{
    deserialized_responses::{ShieldStateCode, SENT_IN_CLEAR},
    latest_event::LatestEvent,
};
use once_cell::sync::Lazy;
use ruma::{
    events::{receipt::Receipt, room::message::MessageType, AnySyncTimelineEvent},
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedMxcUri, OwnedTransactionId,
    OwnedUserId, RoomId, RoomVersionId, TransactionId, UserId,
};
use tracing::warn;

mod content;
mod local;
mod remote;

pub(super) use self::{
    content::{
        extract_bundled_edit_event_json, extract_poll_edit_content, extract_room_msg_edit_content,
        ResponseData,
    },
    local::LocalEventTimelineItem,
    remote::{RemoteEventOrigin, RemoteEventTimelineItem},
};
pub use self::{
    content::{
        AnyOtherFullStateEventContent, EncryptedMessage, InReplyToDetails, MemberProfileChange,
        MembershipChange, Message, OtherState, PollResult, PollState, RepliedToEvent,
        RoomMembershipChange, RoomPinnedEventsChange, Sticker, TimelineItemContent,
    },
    local::EventSendState,
};
use super::{RepliedToInfo, ReplyContent, UnsupportedReplyItem};

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
    /// All bundled reactions about the event.
    pub(super) reactions: ReactionsByKeyBySender,
    /// The timestamp of the event.
    pub(super) timestamp: MilliSecondsSinceUnixEpoch,
    /// The content of the event.
    pub(super) content: TimelineItemContent,
    /// The kind of event timeline item, local or remote.
    pub(super) kind: EventTimelineItemKind,
    /// Whether or not the event belongs to an encrypted room.
    ///
    /// When `None` it is unknown if the room is encrypted and the item won't
    /// return a ShieldState.
    pub(super) is_room_encrypted: Option<bool>,
}

#[derive(Clone, Debug)]
pub(super) enum EventTimelineItemKind {
    /// A local event, not yet echoed back by the server.
    Local(LocalEventTimelineItem),
    /// An event received from the server.
    Remote(RemoteEventTimelineItem),
}

/// A wrapper that can contain either a transaction id, or an event id.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum TimelineEventItemId {
    /// The item is local, identified by its transaction id (to be used in
    /// subsequent requests).
    TransactionId(OwnedTransactionId),
    /// The item is remote, identified by its event id.
    EventId(OwnedEventId),
}

impl From<String> for TimelineEventItemId {
    fn from(value: String) -> Self {
        value.as_str().into()
    }
}

impl From<&str> for TimelineEventItemId {
    fn from(value: &str) -> Self {
        if let Ok(event_id) = EventId::parse(value) {
            TimelineEventItemId::EventId(event_id)
        } else {
            TimelineEventItemId::TransactionId(value.into())
        }
    }
}

/// An handle that usually allows to perform an action on a timeline event.
///
/// If the item represents a remote item, then the event id is usually
/// sufficient to perform an action on it. Otherwise, the send queue handle is
/// returned, if available.
pub(crate) enum TimelineItemHandle<'a> {
    Remote(&'a EventId),
    Local(&'a SendHandle),
}

impl EventTimelineItem {
    pub(super) fn new(
        sender: OwnedUserId,
        sender_profile: TimelineDetails<Profile>,
        timestamp: MilliSecondsSinceUnixEpoch,
        content: TimelineItemContent,
        kind: EventTimelineItemKind,
        reactions: ReactionsByKeyBySender,
        is_room_encrypted: bool,
    ) -> Self {
        let is_room_encrypted = Some(is_room_encrypted);
        Self { sender, sender_profile, timestamp, content, reactions, kind, is_room_encrypted }
    }

    /// If the supplied low-level `SyncTimelineEvent` is suitable for use as the
    /// `latest_event` in a message preview, wrap it as an `EventTimelineItem`.
    ///
    /// **Note:** Timeline items created via this constructor do **not** produce
    /// the correct ShieldState when calling
    /// [`get_shield`][EventTimelineItem::get_shield]. This is because they are
    /// intended for display in the room list which a) is unlikely to show
    /// shields and b) would incur a significant performance overhead.
    pub async fn from_latest_event(
        client: Client,
        room_id: &RoomId,
        latest_event: LatestEvent,
    ) -> Option<EventTimelineItem> {
        // TODO: We shouldn't be returning an EventTimelineItem here because we're
        // starting to diverge on what kind of data we need. The note above is a
        // potential footgun which could one day turn into a security issue.
        use super::traits::RoomDataProvider;

        let raw_sync_event = latest_event.event().raw().clone();
        let encryption_info = latest_event.event().encryption_info().cloned();

        let Ok(event) = raw_sync_event.deserialize_as::<AnySyncTimelineEvent>() else {
            warn!("Unable to deserialize latest_event as an AnySyncTimelineEvent!");
            return None;
        };

        let timestamp = event.origin_server_ts();
        let sender = event.sender().to_owned();
        let event_id = event.event_id().to_owned();
        let is_own = client.user_id().map(|uid| uid == sender).unwrap_or(false);

        // If we don't (yet) know how to handle this type of message, return `None`
        // here. If we do, convert it into a `TimelineItemContent`.
        let content = TimelineItemContent::from_latest_event_content(event)?;

        // We don't currently bundle any reactions with the main event. This could
        // conceivably be wanted in the message preview in future.
        let reactions = ReactionsByKeyBySender::default();

        // The message preview probably never needs read receipts.
        let read_receipts = IndexMap::new();

        // Being highlighted is _probably_ not relevant to the message preview.
        let is_highlighted = false;

        // We may need this, depending on how we are going to display edited messages in
        // previews.
        let latest_edit_json = None;

        // Probably the origin of the event doesn't matter for the preview.
        let origin = RemoteEventOrigin::Sync;

        let kind = RemoteEventTimelineItem {
            event_id,
            transaction_id: None,
            read_receipts,
            is_own,
            is_highlighted,
            encryption_info,
            original_json: Some(raw_sync_event),
            latest_edit_json,
            origin,
        }
        .into();

        let room = client.get_room(room_id);
        let sender_profile = if let Some(room) = room {
            let mut profile = room.profile_from_latest_event(&latest_event);

            // Fallback to the slow path.
            if profile.is_none() {
                profile = room.profile_from_user_id(&sender).await;
            }

            profile.map(TimelineDetails::Ready).unwrap_or(TimelineDetails::Unavailable)
        } else {
            TimelineDetails::Unavailable
        };

        Some(Self {
            sender,
            sender_profile,
            timestamp,
            content,
            kind,
            reactions,
            is_room_encrypted: None,
        })
    }

    /// Check whether this item is a local echo.
    ///
    /// This returns `true` for events created locally, until the server echoes
    /// back the full event as part of a sync response.
    ///
    /// This is the opposite of [`Self::is_remote_event`].
    pub fn is_local_echo(&self) -> bool {
        matches!(self.kind, EventTimelineItemKind::Local(_))
    }

    /// Check whether this item is a remote event.
    ///
    /// This returns `true` only for events that have been echoed back from the
    /// homeserver. A local echo sent but not echoed back yet will return
    /// `false` here.
    ///
    /// This is the opposite of [`Self::is_local_echo`].
    pub fn is_remote_event(&self) -> bool {
        matches!(self.kind, EventTimelineItemKind::Remote(_))
    }

    /// Get the `LocalEventTimelineItem` if `self` is `Local`.
    pub(super) fn as_local(&self) -> Option<&LocalEventTimelineItem> {
        as_variant!(&self.kind, EventTimelineItemKind::Local(local_event_item) => local_event_item)
    }

    /// Get a reference to a [`RemoteEventTimelineItem`] if it's a remote echo.
    pub(super) fn as_remote(&self) -> Option<&RemoteEventTimelineItem> {
        as_variant!(&self.kind, EventTimelineItemKind::Remote(remote_event_item) => remote_event_item)
    }

    /// Get a mutable reference to a [`RemoteEventTimelineItem`] if it's a
    /// remote echo.
    pub(super) fn as_remote_mut(&mut self) -> Option<&mut RemoteEventTimelineItem> {
        as_variant!(&mut self.kind, EventTimelineItemKind::Remote(remote_event_item) => remote_event_item)
    }

    /// Get the event's send state of a local echo.
    pub fn send_state(&self) -> Option<&EventSendState> {
        as_variant!(&self.kind, EventTimelineItemKind::Local(local) => &local.send_state)
    }

    /// Get the unique identifier of this item.
    ///
    /// Returns the transaction ID for a local echo item that has not been sent
    /// and the event ID for a local echo item that has been sent or a
    /// remote item.
    pub fn identifier(&self) -> TimelineEventItemId {
        match &self.kind {
            EventTimelineItemKind::Local(local) => local.identifier(),
            EventTimelineItemKind::Remote(remote) => {
                TimelineEventItemId::EventId(remote.event_id.clone())
            }
        }
    }

    /// Get the transaction ID of a local echo item.
    ///
    /// The transaction ID is currently only kept until the remote echo for a
    /// local event is received.
    pub fn transaction_id(&self) -> Option<&TransactionId> {
        as_variant!(&self.kind, EventTimelineItemKind::Local(local) => &local.transaction_id)
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
    pub fn reactions(&self) -> &ReactionsByKeyBySender {
        &self.reactions
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

    /// Flag indicating this timeline item can be edited by the current user.
    pub fn is_editable(&self) -> bool {
        // Steps here should be in sync with [`EventTimelineItem::edit_info`] and
        // [`Timeline::edit_poll`].

        if !self.is_own() {
            // In theory could work, but it's hard to compute locally.
            return false;
        }

        match self.content() {
            TimelineItemContent::Message(message) => {
                matches!(message.msgtype(), MessageType::Text(_) | MessageType::Emote(_))
            }
            TimelineItemContent::Poll(poll) => {
                poll.response_data.is_empty() && poll.end_event_timestamp.is_none()
            }
            _ => {
                // Other timeline items can't be edited at the moment.
                false
            }
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

    /// Gets the [`ShieldState`] which can be used to decorate messages in the
    /// recommended way.
    pub fn get_shield(&self, strict: bool) -> Option<ShieldState> {
        if self.is_room_encrypted != Some(true) || self.is_local_echo() {
            return None;
        }

        // An unable-to-decrypt message has no authenticity shield.
        if let TimelineItemContent::UnableToDecrypt(_) = self.content() {
            return None;
        }

        match self.encryption_info() {
            Some(info) => {
                if strict {
                    Some(info.verification_state.to_shield_state_strict())
                } else {
                    Some(info.verification_state.to_shield_state_lax())
                }
            }
            None => Some(ShieldState::Red {
                code: ShieldStateCode::SentInClear,
                message: SENT_IN_CLEAR,
            }),
        }
    }

    /// Check whether this item can be replied to.
    pub fn can_be_replied_to(&self) -> bool {
        // This must be in sync with the early returns of `Timeline::send_reply`
        if self.event_id().is_none() {
            false
        } else if let TimelineItemContent::Message(_) = self.content() {
            true
        } else {
            self.latest_json().is_some()
        }
    }

    /// Get the raw JSON representation of the initial event (the one that
    /// caused this timeline item to be created).
    ///
    /// Returns `None` if this event hasn't been echoed back by the server
    /// yet.
    pub fn original_json(&self) -> Option<&Raw<AnySyncTimelineEvent>> {
        match &self.kind {
            EventTimelineItemKind::Local(_) => None,
            EventTimelineItemKind::Remote(remote_event) => remote_event.original_json.as_ref(),
        }
    }

    /// Get the raw JSON representation of the latest edit, if any.
    pub fn latest_edit_json(&self) -> Option<&Raw<AnySyncTimelineEvent>> {
        match &self.kind {
            EventTimelineItemKind::Local(_) => None,
            EventTimelineItemKind::Remote(remote_event) => remote_event.latest_edit_json.as_ref(),
        }
    }

    /// Shorthand for
    /// `item.latest_edit_json().or_else(|| item.original_json())`.
    pub fn latest_json(&self) -> Option<&Raw<AnySyncTimelineEvent>> {
        self.latest_edit_json().or_else(|| self.original_json())
    }

    /// Get the origin of the event, i.e. where it came from.
    ///
    /// May return `None` in some edge cases that are subject to change.
    pub fn origin(&self) -> Option<EventItemOrigin> {
        match &self.kind {
            EventTimelineItemKind::Local(_) => Some(EventItemOrigin::Local),
            EventTimelineItemKind::Remote(remote_event) => match remote_event.origin {
                RemoteEventOrigin::Sync => Some(EventItemOrigin::Sync),
                RemoteEventOrigin::Pagination => Some(EventItemOrigin::Pagination),
                _ => None,
            },
        }
    }

    pub(super) fn set_content(&mut self, content: TimelineItemContent) {
        self.content = content;
    }

    /// Clone the current event item, and update its `kind`.
    pub(super) fn with_kind(&self, kind: impl Into<EventTimelineItemKind>) -> Self {
        Self { kind: kind.into(), ..self.clone() }
    }

    /// Clone the current event item, and update its `reactions`.
    pub fn with_reactions(&self, reactions: ReactionsByKeyBySender) -> Self {
        Self { reactions, ..self.clone() }
    }

    /// Clone the current event item, and update its content.
    ///
    /// Optionally update `latest_edit_json` if the update is an edit received
    /// from the server.
    pub(super) fn with_content(
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

    /// Create a clone of the current item, with content that's been redacted.
    pub(super) fn redact(&self, room_version: &RoomVersionId) -> Self {
        let content = self.content.redact(room_version);
        let kind = match &self.kind {
            EventTimelineItemKind::Local(l) => EventTimelineItemKind::Local(l.clone()),
            EventTimelineItemKind::Remote(r) => EventTimelineItemKind::Remote(r.redact()),
        };
        Self {
            sender: self.sender.clone(),
            sender_profile: self.sender_profile.clone(),
            timestamp: self.timestamp,
            content,
            kind,
            is_room_encrypted: self.is_room_encrypted,
            reactions: ReactionsByKeyBySender::default(),
        }
    }

    /// Gives the information needed to reply to the event of the item.
    pub fn replied_to_info(&self) -> Result<RepliedToInfo, UnsupportedReplyItem> {
        let reply_content = match self.content() {
            TimelineItemContent::Message(msg) => ReplyContent::Message(msg.to_owned()),
            _ => {
                let Some(raw_event) = self.latest_json() else {
                    return Err(UnsupportedReplyItem::MissingJson);
                };

                ReplyContent::Raw(raw_event.clone())
            }
        };

        let Some(event_id) = self.event_id() else {
            return Err(UnsupportedReplyItem::MissingEventId);
        };

        Ok(RepliedToInfo {
            event_id: event_id.to_owned(),
            sender: self.sender().to_owned(),
            timestamp: self.timestamp(),
            content: reply_content,
        })
    }

    pub(super) fn handle(&self) -> TimelineItemHandle<'_> {
        match &self.kind {
            EventTimelineItemKind::Local(local) => {
                if let Some(event_id) = local.event_id() {
                    TimelineItemHandle::Remote(event_id)
                } else {
                    TimelineItemHandle::Local(
                        // The send_handle must always be present, except in tests.
                        local.send_handle.as_ref().expect("Unexpected missing send_handle"),
                    )
                }
            }
            EventTimelineItemKind::Remote(remote) => TimelineItemHandle::Remote(&remote.event_id),
        }
    }
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
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

    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }
}

/// Where this event came.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum EventItemOrigin {
    /// The event was created locally.
    Local,
    /// The event came from a sync response.
    Sync,
    /// The event came from pagination.
    Pagination,
}

/// What's the status of a reaction?
#[derive(Clone, Debug)]
pub enum ReactionStatus {
    /// It's a local reaction to a local echo.
    ///
    /// The handle is missing only in testing contexts.
    LocalToLocal(Option<SendReactionHandle>),
    /// It's a local reaction to a remote event.
    ///
    /// The handle is missing only in testing contexts.
    LocalToRemote(Option<SendHandle>),
    /// It's a remote reaction to a remote event.
    RemoteToRemote(OwnedEventId),
}

/// Information about a single reaction stored in [`ReactionsByKeyBySender`].
#[derive(Clone, Debug)]
pub struct ReactionInfo {
    pub timestamp: MilliSecondsSinceUnixEpoch,
    /// Current status of this reaction.
    pub status: ReactionStatus,
}

/// Reactions grouped by key first, then by sender.
///
/// This representation makes sure that a given sender has sent at most one
/// reaction for an event.
#[derive(Debug, Clone, Default)]
pub struct ReactionsByKeyBySender(IndexMap<String, IndexMap<OwnedUserId, ReactionInfo>>);

impl Deref for ReactionsByKeyBySender {
    type Target = IndexMap<String, IndexMap<OwnedUserId, ReactionInfo>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ReactionsByKeyBySender {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl ReactionsByKeyBySender {
    /// Removes (in place) a reaction from the sender with the given annotation
    /// from the mapping.
    ///
    /// Returns true if the reaction was found and thus removed, false
    /// otherwise.
    pub(crate) fn remove_reaction(
        &mut self,
        sender: &UserId,
        annotation: &str,
    ) -> Option<ReactionInfo> {
        if let Some(by_user) = self.0.get_mut(annotation) {
            if let Some(info) = by_user.swap_remove(sender) {
                // If this was the last reaction, remove the annotation entry.
                if by_user.is_empty() {
                    self.0.swap_remove(annotation);
                }
                return Some(info);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use matrix_sdk::test_utils::{events::EventFactory, logged_in_client};
    use matrix_sdk_base::{
        deserialized_responses::SyncTimelineEvent, latest_event::LatestEvent, sliding_sync::http,
        MinimalStateEvent, OriginalMinimalStateEvent,
    };
    use matrix_sdk_test::{async_test, sync_state_event, sync_timeline_event};
    use ruma::{
        event_id,
        events::{
            room::{
                member::RoomMemberEventContent,
                message::{MessageFormat, MessageType},
            },
            AnySyncStateEvent, AnySyncTimelineEvent, BundledMessageLikeRelations,
        },
        room_id,
        serde::Raw,
        user_id, RoomId, UInt, UserId,
    };

    use super::{EventTimelineItem, Profile};
    use crate::timeline::{TimelineDetails, TimelineEventItemId};

    #[async_test]
    async fn test_latest_message_event_can_be_wrapped_as_a_timeline_item() {
        // Given a sync event that is suitable to be used as a latest_event

        let room_id = room_id!("!q:x.uk");
        let user_id = user_id!("@t:o.uk");
        let event = message_event(room_id, user_id, "**My M**", "<b>My M</b>", 122344);
        let client = logged_in_client(None).await;

        // When we construct a timeline event from it
        let timeline_item =
            EventTimelineItem::from_latest_event(client, room_id, LatestEvent::new(event))
                .await
                .unwrap();

        // Then its properties correctly translate
        assert_eq!(timeline_item.sender, user_id);
        assert_matches!(timeline_item.sender_profile, TimelineDetails::Unavailable);
        assert_eq!(timeline_item.timestamp.0, UInt::new(122344).unwrap());
        if let MessageType::Text(txt) = timeline_item.content.as_message().unwrap().msgtype() {
            assert_eq!(txt.body, "**My M**");
            let formatted = txt.formatted.as_ref().unwrap();
            assert_eq!(formatted.format, MessageFormat::Html);
            assert_eq!(formatted.body, "<b>My M</b>");
        } else {
            panic!("Unexpected message type");
        }
    }

    #[async_test]
    async fn test_latest_message_includes_bundled_edit() {
        // Given a sync event that is suitable to be used as a latest_event, and
        // contains a bundled edit,
        let room_id = room_id!("!q:x.uk");
        let user_id = user_id!("@t:o.uk");

        let f = EventFactory::new();

        let original_event_id = event_id!("$original");

        let mut relations = BundledMessageLikeRelations::new();
        relations.replace = Some(Box::new(
            f.text_html(" * Updated!", " * <b>Updated!</b>")
                .edit(
                    original_event_id,
                    MessageType::text_html("Updated!", "<b>Updated!</b>").into(),
                )
                .event_id(event_id!("$edit"))
                .sender(user_id)
                .into_raw_sync(),
        ));

        let event = f
            .text_html("**My M**", "<b>My M</b>")
            .sender(user_id)
            .event_id(original_event_id)
            .bundled_relations(relations)
            .server_ts(42)
            .into_sync();

        let client = logged_in_client(None).await;

        // When we construct a timeline event from it,
        let timeline_item =
            EventTimelineItem::from_latest_event(client, room_id, LatestEvent::new(event))
                .await
                .unwrap();

        // Then its properties correctly translate.
        assert_eq!(timeline_item.sender, user_id);
        assert_matches!(timeline_item.sender_profile, TimelineDetails::Unavailable);
        assert_eq!(timeline_item.timestamp.0, UInt::new(42).unwrap());
        if let MessageType::Text(txt) = timeline_item.content.as_message().unwrap().msgtype() {
            assert_eq!(txt.body, "Updated!");
            let formatted = txt.formatted.as_ref().unwrap();
            assert_eq!(formatted.format, MessageFormat::Html);
            assert_eq!(formatted.body, "<b>Updated!</b>");
        } else {
            panic!("Unexpected message type");
        }
    }

    #[async_test]
    async fn test_latest_poll_includes_bundled_edit() {
        // Given a sync event that is suitable to be used as a latest_event, and
        // contains a bundled edit,
        let room_id = room_id!("!q:x.uk");
        let user_id = user_id!("@t:o.uk");

        let f = EventFactory::new();

        let original_event_id = event_id!("$original");

        let mut relations = BundledMessageLikeRelations::new();
        relations.replace = Some(Box::new(
            f.poll_edit(
                original_event_id,
                "It's one banana, Michael, how much could it cost?",
                vec!["1 dollar", "10 dollars", "100 dollars"],
            )
            .event_id(event_id!("$edit"))
            .sender(user_id)
            .into_raw_sync(),
        ));

        let event = f
            .poll_start(
                "It's one avocado, Michael, how much could it cost? 10 dollars?",
                "It's one avocado, Michael, how much could it cost?",
                vec!["1 dollar", "10 dollars", "100 dollars"],
            )
            .event_id(original_event_id)
            .bundled_relations(relations)
            .sender(user_id)
            .into_sync();

        let client = logged_in_client(None).await;

        // When we construct a timeline event from it,
        let timeline_item =
            EventTimelineItem::from_latest_event(client, room_id, LatestEvent::new(event))
                .await
                .unwrap();

        // Then its properties correctly translate.
        assert_eq!(timeline_item.sender, user_id);

        let poll = timeline_item.content().as_poll().unwrap();
        assert!(poll.has_been_edited);
        assert_eq!(
            poll.start_event_content.poll_start.question.text,
            "It's one banana, Michael, how much could it cost?"
        );
    }

    #[async_test]
    async fn test_latest_message_event_can_be_wrapped_as_a_timeline_item_with_sender_from_the_storage(
    ) {
        // Given a sync event that is suitable to be used as a latest_event, and a room
        // with a member event for the sender

        use ruma::owned_mxc_uri;
        let room_id = room_id!("!q:x.uk");
        let user_id = user_id!("@t:o.uk");
        let event = message_event(room_id, user_id, "**My M**", "<b>My M</b>", 122344);
        let client = logged_in_client(None).await;
        let mut room = http::response::Room::new();
        room.required_state.push(member_event_as_state_event(
            room_id,
            user_id,
            "Alice Margatroid",
            "mxc://e.org/SEs",
        ));

        // And the room is stored in the client so it can be extracted when needed
        let response = response_with_room(room_id, room);
        client.process_sliding_sync_test_helper(&response).await.unwrap();

        // When we construct a timeline event from it
        let timeline_item =
            EventTimelineItem::from_latest_event(client, room_id, LatestEvent::new(event))
                .await
                .unwrap();

        // Then its sender is properly populated
        assert_let!(TimelineDetails::Ready(profile) = timeline_item.sender_profile);
        assert_eq!(
            profile,
            Profile {
                display_name: Some("Alice Margatroid".to_owned()),
                display_name_ambiguous: false,
                avatar_url: Some(owned_mxc_uri!("mxc://e.org/SEs"))
            }
        );
    }

    #[async_test]
    async fn test_latest_message_event_can_be_wrapped_as_a_timeline_item_with_sender_from_the_cache(
    ) {
        // Given a sync event that is suitable to be used as a latest_event, a room, and
        // a member event for the sender (which isn't part of the room yet).

        use ruma::owned_mxc_uri;
        let room_id = room_id!("!q:x.uk");
        let user_id = user_id!("@t:o.uk");
        let event = message_event(room_id, user_id, "**My M**", "<b>My M</b>", 122344);
        let client = logged_in_client(None).await;

        let member_event = MinimalStateEvent::Original(
            member_event(room_id, user_id, "Alice Margatroid", "mxc://e.org/SEs")
                .deserialize_as::<OriginalMinimalStateEvent<RoomMemberEventContent>>()
                .unwrap(),
        );

        let room = http::response::Room::new();
        // Do not push the `member_event` inside the room. Let's say it's flying in the
        // `StateChanges`.

        // And the room is stored in the client so it can be extracted when needed
        let response = response_with_room(room_id, room);
        client.process_sliding_sync_test_helper(&response).await.unwrap();

        // When we construct a timeline event from it
        let timeline_item = EventTimelineItem::from_latest_event(
            client,
            room_id,
            LatestEvent::new_with_sender_details(event, Some(member_event), None),
        )
        .await
        .unwrap();

        // Then its sender is properly populated
        assert_let!(TimelineDetails::Ready(profile) = timeline_item.sender_profile);
        assert_eq!(
            profile,
            Profile {
                display_name: Some("Alice Margatroid".to_owned()),
                display_name_ambiguous: false,
                avatar_url: Some(owned_mxc_uri!("mxc://e.org/SEs"))
            }
        );
    }

    #[test]
    fn test_raw_event_id_into_timeline_event_item_id_gets_event_id() {
        let raw_id = "$123:example.com";
        let id: TimelineEventItemId = raw_id.into();
        assert_matches!(id, TimelineEventItemId::EventId(_));
    }

    #[test]
    fn test_raw_str_into_timeline_event_item_id_gets_transaction_id() {
        let raw_id = "something something";
        let id: TimelineEventItemId = raw_id.into();
        assert_matches!(id, TimelineEventItemId::TransactionId(_));
    }

    fn member_event(
        room_id: &RoomId,
        user_id: &UserId,
        display_name: &str,
        avatar_url: &str,
    ) -> Raw<AnySyncTimelineEvent> {
        sync_timeline_event!({
            "type": "m.room.member",
            "content": {
                "avatar_url": avatar_url,
                "displayname": display_name,
                "membership": "join",
                "reason": ""
            },
            "event_id": "$143273582443PhrSn:example.org",
            "origin_server_ts": 143273583,
            "room_id": room_id,
            "sender": "@example:example.org",
            "state_key": user_id,
            "type": "m.room.member",
            "unsigned": {
              "age": 1234
            }
        })
    }

    fn member_event_as_state_event(
        room_id: &RoomId,
        user_id: &UserId,
        display_name: &str,
        avatar_url: &str,
    ) -> Raw<AnySyncStateEvent> {
        sync_state_event!({
            "type": "m.room.member",
            "content": {
                "avatar_url": avatar_url,
                "displayname": display_name,
                "membership": "join",
                "reason": ""
            },
            "event_id": "$143273582443PhrSn:example.org",
            "origin_server_ts": 143273583,
            "room_id": room_id,
            "sender": "@example:example.org",
            "state_key": user_id,
            "type": "m.room.member",
            "unsigned": {
              "age": 1234
            }
        })
    }

    fn response_with_room(room_id: &RoomId, room: http::response::Room) -> http::Response {
        let mut response = http::Response::new("6".to_owned());
        response.rooms.insert(room_id.to_owned(), room);
        response
    }

    fn message_event(
        room_id: &RoomId,
        user_id: &UserId,
        body: &str,
        formatted_body: &str,
        ts: u64,
    ) -> SyncTimelineEvent {
        sync_timeline_event!({
            "event_id": "$eventid6",
            "sender": user_id,
            "origin_server_ts": ts,
            "type": "m.room.message",
            "room_id": room_id.to_string(),
            "content": {
                "body": body,
                "format": "org.matrix.custom.html",
                "formatted_body": formatted_body,
                "msgtype": "m.text"
            },
        })
        .into()
    }
}
