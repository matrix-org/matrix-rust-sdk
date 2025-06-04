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
use unicode_segmentation::UnicodeSegmentation;

mod content;
mod local;
mod remote;

pub(super) use self::{
    content::{
        extract_bundled_edit_event_json, extract_poll_edit_content, extract_room_msg_edit_content,
    },
    local::LocalEventTimelineItem,
    remote::{RemoteEventOrigin, RemoteEventTimelineItem},
};
pub use self::{
    content::{
        AnyOtherFullStateEventContent, EmbeddedEvent, EncryptedMessage, InReplyToDetails,
        MemberProfileChange, MembershipChange, Message, MsgLikeContent, MsgLikeKind, OtherState,
        PollResult, PollState, RoomMembershipChange, RoomPinnedEventsChange, Sticker,
        ThreadSummary, TimelineItemContent,
    },
    local::EventSendState,
};

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
    /// Whether or not the event belongs to an encrypted room.
    ///
    /// May be false when we don't know about the room encryption status yet.
    pub(super) is_room_encrypted: bool,
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
        is_room_encrypted: bool,
    ) -> Self {
        Self { sender, sender_profile, timestamp, content, kind, is_room_encrypted }
    }

    /// If the supplied low-level [`TimelineEvent`] is suitable for use as the
    /// `latest_event` in a message preview, wrap it as an
    /// `EventTimelineItem`.
    ///
    /// **Note:** Timeline items created via this constructor do **not** produce
    /// the correct ShieldState when calling
    /// [`get_shield`][EventTimelineItem::get_shield]. This is because they are
    /// intended for display in the room list which a) is unlikely to show
    /// shields and b) would incur a significant performance overhead.
    ///
    /// [`TimelineEvent`]: matrix_sdk::deserialized_responses::TimelineEvent
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

        // Get the room's power levels for calculating the latest event
        let power_levels = if let Some(room) = client.get_room(room_id) {
            room.power_levels().await.ok()
        } else {
            None
        };
        let room_power_levels_info = client.user_id().zip(power_levels.as_ref());

        // If we don't (yet) know how to handle this type of message, return `None`
        // here. If we do, convert it into a `TimelineItemContent`.
        let content =
            TimelineItemContent::from_latest_event_content(event, room_power_levels_info)?;

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

        Some(Self { sender, sender_profile, timestamp, content, kind, is_room_encrypted: false })
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

    /// Get the time that the local event was pushed in the send queue at.
    pub fn local_created_at(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        match &self.kind {
            EventTimelineItemKind::Local(local) => local.send_handle.as_ref().map(|s| s.created_at),
            EventTimelineItemKind::Remote(_) => None,
        }
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

    /// Get a mutable handle to the content of this item.
    pub(crate) fn content_mut(&mut self) -> &mut TimelineItemContent {
        &mut self.content
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
            TimelineItemContent::MsgLike(msglike) => match &msglike.kind {
                MsgLikeKind::Message(message) => match message.msgtype() {
                    MessageType::Text(_)
                    | MessageType::Emote(_)
                    | MessageType::Audio(_)
                    | MessageType::File(_)
                    | MessageType::Image(_)
                    | MessageType::Video(_) => true,
                    #[cfg(feature = "unstable-msc4274")]
                    MessageType::Gallery(_) => true,
                    _ => false,
                },
                MsgLikeKind::Poll(poll) => {
                    poll.response_data.is_empty() && poll.end_event_timestamp.is_none()
                }
                // Other MsgLike timeline items can't be edited at the moment.
                _ => false,
            },
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
            EventTimelineItemKind::Remote(remote_event) => remote_event.encryption_info.as_deref(),
        }
    }

    /// Gets the [`ShieldState`] which can be used to decorate messages in the
    /// recommended way.
    pub fn get_shield(&self, strict: bool) -> Option<ShieldState> {
        if !self.is_room_encrypted || self.is_local_echo() {
            return None;
        }

        // An unable-to-decrypt message has no authenticity shield.
        if self.content().is_unable_to_decrypt() {
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
        } else if self.content.is_message() {
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
                RemoteEventOrigin::Cache => Some(EventItemOrigin::Cache),
                RemoteEventOrigin::Unknown => None,
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

    /// Clone the current event item, and update its content.
    pub(super) fn with_content(&self, new_content: TimelineItemContent) -> Self {
        let mut new = self.clone();
        new.content = new_content;
        new
    }

    /// Clone the current event item, and update its content.
    ///
    /// Optionally update `latest_edit_json` if the update is an edit received
    /// from the server.
    pub(super) fn with_content_and_latest_edit(
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

    /// Clone the current event item, and update its `encryption_info`.
    pub(super) fn with_encryption_info(
        &self,
        encryption_info: Option<Arc<EncryptionInfo>>,
    ) -> Self {
        let mut new = self.clone();
        if let EventTimelineItemKind::Remote(r) = &mut new.kind {
            r.encryption_info = encryption_info;
        }

        new
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
        }
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

    /// For local echoes, return the associated send handle.
    pub fn local_echo_send_handle(&self) -> Option<SendHandle> {
        as_variant!(self.handle(), TimelineItemHandle::Local(handle) => handle.clone())
    }

    /// Some clients may want to know if a particular text message or media
    /// caption contains only emojis so that they can render them bigger for
    /// added effect.
    ///
    /// This function provides that feature with the following
    /// behavior/limitations:
    /// - ignores leading and trailing white spaces
    /// - fails texts bigger than 5 graphemes for performance reasons
    /// - checks the body only for [`MessageType::Text`]
    /// - only checks the caption for [`MessageType::Audio`],
    ///   [`MessageType::File`], [`MessageType::Image`], and
    ///   [`MessageType::Video`] if present
    /// - all other message types will not match
    ///
    /// # Examples
    /// # fn render_timeline_item(timeline_item: TimelineItem) {
    /// if timeline_item.contains_only_emojis() {
    ///     // e.g. increase the font size
    /// }
    /// # }
    ///
    /// See `test_emoji_detection` for more examples.
    pub fn contains_only_emojis(&self) -> bool {
        let body = match self.content() {
            TimelineItemContent::MsgLike(msglike) => match &msglike.kind {
                MsgLikeKind::Message(message) => match &message.msgtype {
                    MessageType::Text(text) => Some(text.body.as_str()),
                    MessageType::Audio(audio) => audio.caption(),
                    MessageType::File(file) => file.caption(),
                    MessageType::Image(image) => image.caption(),
                    MessageType::Video(video) => video.caption(),
                    _ => None,
                },
                MsgLikeKind::Sticker(_)
                | MsgLikeKind::Poll(_)
                | MsgLikeKind::Redacted
                | MsgLikeKind::UnableToDecrypt(_) => None,
            },
            TimelineItemContent::MembershipChange(_)
            | TimelineItemContent::ProfileChange(_)
            | TimelineItemContent::OtherState(_)
            | TimelineItemContent::FailedToParseMessageLike { .. }
            | TimelineItemContent::FailedToParseState { .. }
            | TimelineItemContent::CallInvite
            | TimelineItemContent::CallNotify => None,
        };

        if let Some(body) = body {
            // Collect the graphemes after trimming white spaces.
            let graphemes = body.trim().graphemes(true).collect::<Vec<&str>>();

            // Limit the check to 5 graphemes for performance and security
            // reasons. This will probably be used for every new message so we
            // want it to be fast and we don't want to allow a DoS attack by
            // sending a huge message.
            if graphemes.len() > 5 {
                return false;
            }

            graphemes.iter().all(|g| emojis::get(g).is_some())
        } else {
            false
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
    /// The details are not available yet, and have not been requested from the
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

    pub fn is_unavailable(&self) -> bool {
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
    /// The event came from a cache.
    Cache,
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
    ///
    /// The event id is that of the reaction event (not the target event).
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
    use matrix_sdk::test_utils::logged_in_client;
    use matrix_sdk_base::{
        deserialized_responses::TimelineEvent, latest_event::LatestEvent, MinimalStateEvent,
        OriginalMinimalStateEvent, RequestedRequiredStates,
    };
    use matrix_sdk_test::{async_test, event_factory::EventFactory, sync_state_event};
    use ruma::{
        api::client::sync::sync_events::v5 as http,
        event_id,
        events::{
            room::{
                member::RoomMemberEventContent,
                message::{MessageFormat, MessageType},
            },
            AnySyncStateEvent,
        },
        room_id,
        serde::Raw,
        user_id, RoomId, UInt, UserId,
    };

    use super::{EventTimelineItem, Profile};
    use crate::timeline::{MembershipChange, TimelineDetails, TimelineItemContent};

    #[async_test]
    async fn test_latest_message_event_can_be_wrapped_as_a_timeline_item() {
        // Given a sync event that is suitable to be used as a latest_event

        let room_id = room_id!("!q:x.uk");
        let user_id = user_id!("@t:o.uk");
        let event = EventFactory::new()
            .room(room_id)
            .text_html("**My M**", "<b>My M</b>")
            .sender(user_id)
            .server_ts(122344)
            .into_event();
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
    async fn test_latest_knock_member_state_event_can_be_wrapped_as_a_timeline_item() {
        // Given a sync knock member state event that is suitable to be used as a
        // latest_event

        let room_id = room_id!("!q:x.uk");
        let user_id = user_id!("@t:o.uk");
        let raw_event = member_event_as_state_event(
            room_id,
            user_id,
            "knock",
            "Alice Margatroid",
            "mxc://e.org/SEs",
        );
        let client = logged_in_client(None).await;

        // Add power levels state event, otherwise the knock state event can't be used
        // as the latest event
        let power_level_event = sync_state_event!({
            "type": "m.room.power_levels",
            "content": {},
            "event_id": "$143278582443PhrSn:example.org",
            "origin_server_ts": 143273581,
            "room_id": room_id,
            "sender": user_id,
            "state_key": "",
            "unsigned": {
              "age": 1234
            }
        });
        let mut room = http::response::Room::new();
        room.required_state.push(power_level_event);

        // And the room is stored in the client so it can be extracted when needed
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync_test_helper(&response, &RequestedRequiredStates::default())
            .await
            .unwrap();

        // When we construct a timeline event from it
        let event = TimelineEvent::from_plaintext(raw_event.cast());
        let timeline_item =
            EventTimelineItem::from_latest_event(client, room_id, LatestEvent::new(event))
                .await
                .unwrap();

        // Then its properties correctly translate
        assert_eq!(timeline_item.sender, user_id);
        assert_matches!(timeline_item.sender_profile, TimelineDetails::Unavailable);
        assert_eq!(timeline_item.timestamp.0, UInt::new(143273583).unwrap());
        if let TimelineItemContent::MembershipChange(change) = timeline_item.content {
            assert_eq!(change.user_id, user_id);
            assert_matches!(change.change, Some(MembershipChange::Knocked));
        } else {
            panic!("Unexpected state event type");
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

        let event = f
            .text_html("**My M**", "<b>My M</b>")
            .sender(user_id)
            .event_id(original_event_id)
            .with_bundled_edit(
                f.text_html(" * Updated!", " * <b>Updated!</b>")
                    .edit(
                        original_event_id,
                        MessageType::text_html("Updated!", "<b>Updated!</b>").into(),
                    )
                    .event_id(event_id!("$edit"))
                    .sender(user_id),
            )
            .server_ts(42)
            .into_event();

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

        let event = f
            .poll_start(
                "It's one avocado, Michael, how much could it cost? 10 dollars?",
                "It's one avocado, Michael, how much could it cost?",
                vec!["1 dollar", "10 dollars", "100 dollars"],
            )
            .event_id(original_event_id)
            .with_bundled_edit(
                f.poll_edit(
                    original_event_id,
                    "It's one banana, Michael, how much could it cost?",
                    vec!["1 dollar", "10 dollars", "100 dollars"],
                )
                .event_id(event_id!("$edit"))
                .sender(user_id),
            )
            .sender(user_id)
            .into_event();

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
        let event = EventFactory::new()
            .room(room_id)
            .text_html("**My M**", "<b>My M</b>")
            .sender(user_id)
            .into_event();
        let client = logged_in_client(None).await;
        let mut room = http::response::Room::new();
        room.required_state.push(member_event_as_state_event(
            room_id,
            user_id,
            "join",
            "Alice Margatroid",
            "mxc://e.org/SEs",
        ));

        // And the room is stored in the client so it can be extracted when needed
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync_test_helper(&response, &RequestedRequiredStates::default())
            .await
            .unwrap();

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
        let f = EventFactory::new().room(room_id);
        let event = f.text_html("**My M**", "<b>My M</b>").sender(user_id).into_event();
        let client = logged_in_client(None).await;

        let member_event = MinimalStateEvent::Original(
            f.member(user_id)
                .sender(user_id!("@example:example.org"))
                .avatar_url("mxc://e.org/SEs".into())
                .display_name("Alice Margatroid")
                .reason("")
                .into_raw_sync()
                .deserialize_as::<OriginalMinimalStateEvent<RoomMemberEventContent>>()
                .unwrap(),
        );

        let room = http::response::Room::new();
        // Do not push the `member_event` inside the room. Let's say it's flying in the
        // `StateChanges`.

        // And the room is stored in the client so it can be extracted when needed
        let response = response_with_room(room_id, room);
        client
            .process_sliding_sync_test_helper(&response, &RequestedRequiredStates::default())
            .await
            .unwrap();

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

    #[async_test]
    async fn test_emoji_detection() {
        let room_id = room_id!("!q:x.uk");
        let user_id = user_id!("@t:o.uk");
        let client = logged_in_client(None).await;
        let f = EventFactory::new().room(room_id).sender(user_id);

        let mut event = f.text_html("🤷‍♂️ No boost 🤷‍♂️", "").into_event();
        let mut timeline_item =
            EventTimelineItem::from_latest_event(client.clone(), room_id, LatestEvent::new(event))
                .await
                .unwrap();

        assert!(!timeline_item.contains_only_emojis());

        // Ignores leading and trailing white spaces
        event = f.text_html(" 🚀 ", "").into_event();
        timeline_item =
            EventTimelineItem::from_latest_event(client.clone(), room_id, LatestEvent::new(event))
                .await
                .unwrap();

        assert!(timeline_item.contains_only_emojis());

        // Too many
        event = f.text_html("👨‍👩‍👦1️⃣🚀👳🏾‍♂️🪩👍👍🏻🫱🏼‍🫲🏾🙂👋", "").into_event();
        timeline_item =
            EventTimelineItem::from_latest_event(client.clone(), room_id, LatestEvent::new(event))
                .await
                .unwrap();

        assert!(!timeline_item.contains_only_emojis());

        // Works with combined emojis
        event = f.text_html("👨‍👩‍👦1️⃣👳🏾‍♂️👍🏻🫱🏼‍🫲🏾", "").into_event();
        timeline_item =
            EventTimelineItem::from_latest_event(client.clone(), room_id, LatestEvent::new(event))
                .await
                .unwrap();

        assert!(timeline_item.contains_only_emojis());
    }

    fn member_event_as_state_event(
        room_id: &RoomId,
        user_id: &UserId,
        membership: &str,
        display_name: &str,
        avatar_url: &str,
    ) -> Raw<AnySyncStateEvent> {
        sync_state_event!({
            "type": "m.room.member",
            "content": {
                "avatar_url": avatar_url,
                "displayname": display_name,
                "membership": membership,
                "reason": ""
            },
            "event_id": "$143273582443PhrSn:example.org",
            "origin_server_ts": 143273583,
            "room_id": room_id,
            "sender": user_id,
            "state_key": user_id,
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
}
