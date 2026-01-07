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
    Error,
    deserialized_responses::{EncryptionInfo, ShieldState},
    send_queue::{SendHandle, SendReactionHandle},
};
use matrix_sdk_base::deserialized_responses::ShieldStateCode;
use once_cell::sync::Lazy;
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedMxcUri, OwnedTransactionId,
    OwnedUserId, TransactionId, UserId,
    events::{AnySyncTimelineEvent, receipt::Receipt, room::message::MessageType},
    room_version_rules::RedactionRules,
    serde::Raw,
};
use unicode_segmentation::UnicodeSegmentation;

mod content;
mod local;
mod remote;

pub use self::{
    content::{
        AnyOtherFullStateEventContent, EmbeddedEvent, EncryptedMessage, InReplyToDetails,
        MemberProfileChange, MembershipChange, Message, MsgLikeContent, MsgLikeKind,
        OtherMessageLike, OtherState, PollResult, PollState, RoomMembershipChange,
        RoomPinnedEventsChange, Sticker, ThreadSummary, TimelineItemContent,
    },
    local::{EventSendState, MediaUploadProgress},
};
pub(super) use self::{
    content::{
        extract_bundled_edit_event_json, extract_poll_edit_content, extract_room_msg_edit_content,
    },
    local::LocalEventTimelineItem,
    remote::{RemoteEventOrigin, RemoteEventTimelineItem},
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
    /// If the keys used to decrypt this event were shared-on-invite as part of
    /// an [MSC4268] key bundle, the user ID of the forwarder.
    ///
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    pub(super) forwarder: Option<OwnedUserId>,
    /// If the keys used to decrypt this event were shared-on-invite as part of
    /// an [MSC4268] key bundle, the forwarder's profile, if present.
    ///
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    pub(super) forwarder_profile: Option<TimelineDetails<Profile>>,
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
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        sender: OwnedUserId,
        sender_profile: TimelineDetails<Profile>,
        forwarder: Option<OwnedUserId>,
        forwarder_profile: Option<TimelineDetails<Profile>>,
        timestamp: MilliSecondsSinceUnixEpoch,
        content: TimelineItemContent,
        kind: EventTimelineItemKind,
        is_room_encrypted: bool,
    ) -> Self {
        Self {
            sender,
            sender_profile,
            forwarder,
            forwarder_profile,
            timestamp,
            content,
            kind,
            is_room_encrypted,
        }
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

    /// If the keys used to decrypt this event were shared-on-invite as part of
    /// an [MSC4268] key bundle, returns the user ID of the forwarder.
    ///
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    pub fn forwarder(&self) -> Option<&UserId> {
        self.forwarder.as_deref()
    }

    /// If the keys used to decrypt this event were shared-on-invite as part of
    /// an [MSC4268] key bundle, returns the profile of the forwarder.
    ///
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    pub fn forwarder_profile(&self) -> Option<&TimelineDetails<Profile>> {
        self.forwarder_profile.as_ref()
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

    /// Gets the [`TimelineEventShieldState`] which can be used to decorate
    /// messages in the recommended way.
    pub fn get_shield(&self, strict: bool) -> TimelineEventShieldState {
        if !self.is_room_encrypted || self.is_local_echo() {
            return TimelineEventShieldState::None;
        }

        // An unable-to-decrypt message has no authenticity shield.
        if self.content().is_unable_to_decrypt() {
            return TimelineEventShieldState::None;
        }

        match self.encryption_info() {
            Some(info) => {
                if strict {
                    info.verification_state.to_shield_state_strict().into()
                } else {
                    info.verification_state.to_shield_state_lax().into()
                }
            }
            None => {
                TimelineEventShieldState::Red { code: TimelineEventShieldStateCode::SentInClear }
            }
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
    pub(super) fn redact(&self, rules: &RedactionRules) -> Self {
        let content = self.content.redact(rules);
        let kind = match &self.kind {
            EventTimelineItemKind::Local(l) => EventTimelineItemKind::Local(l.clone()),
            EventTimelineItemKind::Remote(r) => EventTimelineItemKind::Remote(r.redact()),
        };
        Self {
            sender: self.sender.clone(),
            sender_profile: self.sender_profile.clone(),
            forwarder: self.forwarder.clone(),
            forwarder_profile: self.forwarder_profile.clone(),
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
                | MsgLikeKind::UnableToDecrypt(_)
                | MsgLikeKind::Other(_) => None,
            },
            TimelineItemContent::MembershipChange(_)
            | TimelineItemContent::ProfileChange(_)
            | TimelineItemContent::OtherState(_)
            | TimelineItemContent::FailedToParseMessageLike { .. }
            | TimelineItemContent::FailedToParseState { .. }
            | TimelineItemContent::CallInvite
            | TimelineItemContent::RtcNotification => None,
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
        if let Some(by_user) = self.0.get_mut(annotation)
            && let Some(info) = by_user.swap_remove(sender)
        {
            // If this was the last reaction, remove the annotation entry.
            if by_user.is_empty() {
                self.0.swap_remove(annotation);
            }
            return Some(info);
        }
        None
    }
}

/// Extends [`ShieldState`] to allow for a `SentInClear` code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineEventShieldState {
    /// A red shield with a tooltip containing a message appropriate to the
    /// associated code should be presented.
    Red {
        /// A machine-readable representation.
        code: TimelineEventShieldStateCode,
    },
    /// A grey shield with a tooltip containing a message appropriate to the
    /// associated code should be presented.
    Grey {
        /// A machine-readable representation.
        code: TimelineEventShieldStateCode,
    },
    /// No shield should be presented.
    None,
}

impl From<ShieldState> for TimelineEventShieldState {
    fn from(value: ShieldState) -> Self {
        match value {
            ShieldState::Red { code, message: _ } => {
                TimelineEventShieldState::Red { code: code.into() }
            }
            ShieldState::Grey { code, message: _ } => {
                TimelineEventShieldState::Grey { code: code.into() }
            }
            ShieldState::None => TimelineEventShieldState::None,
        }
    }
}

/// Extends [`ShieldStateCode`] to allow for a `SentInClear` code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum TimelineEventShieldStateCode {
    /// Not enough information available to check the authenticity.
    AuthenticityNotGuaranteed,
    /// The sending device isn't yet known by the Client.
    UnknownDevice,
    /// The sending device hasn't been verified by the sender.
    UnsignedDevice,
    /// The sender hasn't been verified by the Client's user.
    UnverifiedIdentity,
    /// The sender was previously verified but changed their identity.
    VerificationViolation,
    /// The `sender` field on the event does not match the owner of the device
    /// that established the Megolm session.
    MismatchedSender,
    /// An unencrypted event in an encrypted room.
    SentInClear,
}

impl From<ShieldStateCode> for TimelineEventShieldStateCode {
    fn from(value: ShieldStateCode) -> Self {
        use TimelineEventShieldStateCode::*;
        match value {
            ShieldStateCode::AuthenticityNotGuaranteed => AuthenticityNotGuaranteed,
            ShieldStateCode::UnknownDevice => UnknownDevice,
            ShieldStateCode::UnsignedDevice => UnsignedDevice,
            ShieldStateCode::UnverifiedIdentity => UnverifiedIdentity,
            ShieldStateCode::VerificationViolation => VerificationViolation,
            ShieldStateCode::MismatchedSender => MismatchedSender,
        }
    }
}
