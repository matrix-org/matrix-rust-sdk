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

#![allow(missing_docs)]

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicU64, Ordering::SeqCst},
    time::Duration,
};

use as_variant::as_variant;
use matrix_sdk_common::deserialized_responses::{
    TimelineEvent, UnableToDecryptInfo, UnableToDecryptReason,
};
use ruma::{
    EventId, Int, MilliSecondsSinceUnixEpoch, MxcUri, OwnedDeviceId, OwnedEventId, OwnedMxcUri,
    OwnedRoomAliasId, OwnedRoomId, OwnedTransactionId, OwnedUserId, OwnedVoipId, RoomId,
    RoomVersionId, TransactionId, UInt, UserId, VoipVersionId,
    events::{
        AnyGlobalAccountDataEvent, AnyMessageLikeEvent, AnyStateEvent, AnyStrippedStateEvent,
        AnySyncEphemeralRoomEvent, AnySyncMessageLikeEvent, AnySyncStateEvent,
        AnySyncTimelineEvent, AnyTimelineEvent, BundledMessageLikeRelations,
        EphemeralRoomEventContent, EventContentFromType, False, GlobalAccountDataEventContent,
        Mentions, MessageLikeEvent, MessageLikeEventContent, PossiblyRedactedStateEventContent,
        RedactContent, RedactedMessageLikeEventContent, RedactedStateEventContent, StateEvent,
        StateEventContent, StaticEventContent, StaticStateEventContent, StrippedStateEvent,
        SyncMessageLikeEvent, SyncStateEvent,
        beacon::BeaconEventContent,
        call::{SessionDescription, invite::CallInviteEventContent},
        direct::{DirectEventContent, OwnedDirectUserIdentifier},
        ignored_user_list::IgnoredUserListEventContent,
        macros::EventContent,
        member_hints::MemberHintsEventContent,
        poll::{
            unstable_end::UnstablePollEndEventContent,
            unstable_response::UnstablePollResponseEventContent,
            unstable_start::{
                NewUnstablePollStartEventContent, ReplacementUnstablePollStartEventContent,
                UnstablePollAnswer, UnstablePollStartContentBlock, UnstablePollStartEventContent,
            },
        },
        push_rules::PushRulesEventContent,
        reaction::ReactionEventContent,
        receipt::{Receipt, ReceiptEventContent, ReceiptThread, ReceiptType},
        relation::{Annotation, BundledThread, InReplyTo, Reference, Replacement, Thread},
        room::{
            ImageInfo,
            avatar::{self, RoomAvatarEventContent},
            canonical_alias::RoomCanonicalAliasEventContent,
            create::{PreviousRoom, RoomCreateEventContent},
            encrypted::{
                EncryptedEventScheme, MegolmV1AesSha2ContentInit, RoomEncryptedEventContent,
            },
            member::{MembershipState, RoomMemberEventContent},
            message::{
                FormattedBody, GalleryItemType, GalleryMessageEventContent,
                ImageMessageEventContent, MessageType, OriginalSyncRoomMessageEvent, Relation,
                RelationWithoutReplacement, RoomMessageEventContent,
                RoomMessageEventContentWithoutRelation,
            },
            name::RoomNameEventContent,
            power_levels::RoomPowerLevelsEventContent,
            redaction::RoomRedactionEventContent,
            server_acl::RoomServerAclEventContent,
            tombstone::RoomTombstoneEventContent,
            topic::RoomTopicEventContent,
        },
        rtc::{
            decline::RtcDeclineEventContent,
            notification::{NotificationType, RtcNotificationEventContent},
        },
        space::{child::SpaceChildEventContent, parent::SpaceParentEventContent},
        sticker::StickerEventContent,
        typing::TypingEventContent,
    },
    push::Ruleset,
    room::RoomType,
    room_version_rules::AuthorizationRules,
    serde::Raw,
    server_name,
};
use serde::Serialize;
use serde_json::json;

pub trait TimestampArg {
    fn to_milliseconds_since_unix_epoch(self) -> MilliSecondsSinceUnixEpoch;
}

impl TimestampArg for MilliSecondsSinceUnixEpoch {
    fn to_milliseconds_since_unix_epoch(self) -> MilliSecondsSinceUnixEpoch {
        self
    }
}

impl TimestampArg for u64 {
    fn to_milliseconds_since_unix_epoch(self) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch(UInt::try_from(self).unwrap())
    }
}

/// A thin copy of [`ruma::events::UnsignedRoomRedactionEvent`].
#[derive(Debug, Serialize)]
struct RedactedBecause {
    /// Data specific to the event type.
    content: RoomRedactionEventContent,

    /// The globally unique event identifier for the user who sent the event.
    event_id: OwnedEventId,

    /// The fully-qualified ID of the user who sent this event.
    sender: OwnedUserId,

    /// Timestamp in milliseconds on originating homeserver when this event was
    /// sent.
    origin_server_ts: MilliSecondsSinceUnixEpoch,
}

#[derive(Debug, Serialize)]
struct Unsigned<C: StaticEventContent> {
    #[serde(skip_serializing_if = "Option::is_none")]
    prev_content: Option<C>,

    #[serde(skip_serializing_if = "Option::is_none")]
    transaction_id: Option<OwnedTransactionId>,

    #[serde(rename = "m.relations", skip_serializing_if = "Option::is_none")]
    relations: Option<BundledMessageLikeRelations<Raw<AnySyncTimelineEvent>>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    redacted_because: Option<RedactedBecause>,

    #[serde(skip_serializing_if = "Option::is_none")]
    age: Option<Int>,
}

// rustc can't derive Default because C isn't marked as `Default` 🤔 oh well.
impl<C: StaticEventContent> Default for Unsigned<C> {
    fn default() -> Self {
        Self {
            prev_content: None,
            transaction_id: None,
            relations: None,
            redacted_because: None,
            age: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum EventFormat {
    /// An event that can be received in the timeline, via `/messages` for
    /// example.
    #[default]
    Timeline,
    /// An event that can be received in the timeline, via `/sync`.
    SyncTimeline,
    /// An event that is received in stripped state.
    StrippedState,
    /// An ephemeral event, like a read receipt.
    Ephemeral,
    /// A global account data.
    GlobalAccountData,
}

impl EventFormat {
    /// Whether this format has a `sender` field.
    fn has_sender(self) -> bool {
        matches!(self, Self::Timeline | Self::SyncTimeline | Self::StrippedState)
    }

    /// Whether this format has an `event_id` field.
    fn has_event_id(self) -> bool {
        matches!(self, Self::Timeline | Self::SyncTimeline)
    }

    /// Whether this format ha an `room_id` field.
    fn has_room_id(self) -> bool {
        matches!(self, Self::Timeline)
    }
}

#[derive(Debug)]
pub struct EventBuilder<C: StaticEventContent<IsPrefix = False>> {
    /// The format of the event.
    ///
    /// It will decide which fields are added to the JSON.
    format: EventFormat,
    sender: Option<OwnedUserId>,
    room: Option<OwnedRoomId>,
    event_id: Option<OwnedEventId>,
    /// Whether the event should *not* have an event id. False by default.
    no_event_id: bool,
    redacts: Option<OwnedEventId>,
    content: C,
    server_ts: MilliSecondsSinceUnixEpoch,
    unsigned: Option<Unsigned<C>>,
    state_key: Option<String>,
}

impl<E: StaticEventContent<IsPrefix = False>> EventBuilder<E> {
    fn format(mut self, format: EventFormat) -> Self {
        self.format = format;
        self
    }

    pub fn room(mut self, room_id: &RoomId) -> Self {
        self.room = Some(room_id.to_owned());
        self
    }

    pub fn sender(mut self, sender: &UserId) -> Self {
        self.sender = Some(sender.to_owned());
        self
    }

    pub fn event_id(mut self, event_id: &EventId) -> Self {
        self.event_id = Some(event_id.to_owned());
        self.no_event_id = false;
        self
    }

    pub fn no_event_id(mut self) -> Self {
        self.event_id = None;
        self.no_event_id = true;
        self
    }

    pub fn server_ts(mut self, ts: impl TimestampArg) -> Self {
        self.server_ts = ts.to_milliseconds_since_unix_epoch();
        self
    }

    pub fn unsigned_transaction_id(mut self, transaction_id: &TransactionId) -> Self {
        self.unsigned.get_or_insert_with(Default::default).transaction_id =
            Some(transaction_id.to_owned());
        self
    }

    /// Add age to unsigned data in this event.
    pub fn age(mut self, age: impl Into<Int>) -> Self {
        self.unsigned.get_or_insert_with(Default::default).age = Some(age.into());
        self
    }

    /// Create a bundled thread summary in the unsigned bundled relations of
    /// this event.
    pub fn with_bundled_thread_summary(
        mut self,
        latest_event: Raw<AnySyncMessageLikeEvent>,
        count: usize,
        current_user_participated: bool,
    ) -> Self {
        let relations = self
            .unsigned
            .get_or_insert_with(Default::default)
            .relations
            .get_or_insert_with(BundledMessageLikeRelations::new);
        relations.thread = Some(Box::new(BundledThread::new(
            latest_event,
            UInt::try_from(count).unwrap(),
            current_user_participated,
        )));
        self
    }

    /// Create a bundled edit in the unsigned bundled relations of this event.
    pub fn with_bundled_edit(mut self, replacement: impl Into<Raw<AnySyncTimelineEvent>>) -> Self {
        let relations = self
            .unsigned
            .get_or_insert_with(Default::default)
            .relations
            .get_or_insert_with(BundledMessageLikeRelations::new);
        relations.replace = Some(Box::new(replacement.into()));
        self
    }

    /// For state events manually created, define the state key.
    ///
    /// For other state events created in the [`EventFactory`], this is
    /// automatically filled upon creation or update of the events.
    pub fn state_key(mut self, state_key: impl Into<String>) -> Self {
        self.state_key = Some(state_key.into());
        self
    }
}

impl<E> EventBuilder<E>
where
    E: StaticEventContent<IsPrefix = False> + Serialize,
{
    #[inline(always)]
    fn construct_json(self) -> serde_json::Value {
        let mut json = json!({
            "type": E::TYPE,
            "content": self.content,
            "origin_server_ts": self.server_ts,
        });

        let map = json.as_object_mut().unwrap();

        if self.format.has_sender() {
            // Use the `sender` preferably, or resort to the `redacted_because` sender if
            // none has been set.
            let sender = self
                .sender
                .or_else(|| Some(self.unsigned.as_ref()?.redacted_because.as_ref()?.sender.clone())).expect("the sender must be known when building the JSON for a non read-receipt or global event");
            map.insert("sender".to_owned(), json!(sender));
        }

        if self.format.has_event_id() && !self.no_event_id {
            let event_id = self.event_id.unwrap_or_else(|| {
                let server_name = self
                    .room
                    .as_ref()
                    .and_then(|room_id| room_id.server_name())
                    .unwrap_or(server_name!("dummy.org"));

                EventId::new(server_name)
            });

            map.insert("event_id".to_owned(), json!(event_id));
        }

        if self.format.has_room_id() {
            let room_id = self.room.expect("TimelineEvent requires a room id");
            map.insert("room_id".to_owned(), json!(room_id));
        }

        if let Some(redacts) = self.redacts {
            map.insert("redacts".to_owned(), json!(redacts));
        }

        if let Some(unsigned) = self.unsigned {
            map.insert("unsigned".to_owned(), json!(unsigned));
        }

        if let Some(state_key) = self.state_key {
            map.insert("state_key".to_owned(), json!(state_key));
        }

        json
    }

    /// Build an event from the [`EventBuilder`] and convert it into a
    /// serialized and [`Raw`] event.
    ///
    /// The generic argument `T` allows you to automatically cast the [`Raw`]
    /// event into any desired type.
    pub fn into_raw<T>(self) -> Raw<T> {
        Raw::new(&self.construct_json()).unwrap().cast_unchecked()
    }

    pub fn into_raw_timeline(self) -> Raw<AnyTimelineEvent> {
        self.into_raw()
    }

    pub fn into_any_sync_message_like_event(self) -> AnySyncMessageLikeEvent {
        self.format(EventFormat::SyncTimeline)
            .into_raw()
            .deserialize()
            .expect("expected message like event")
    }

    pub fn into_original_sync_room_message_event(self) -> OriginalSyncRoomMessageEvent {
        self.format(EventFormat::SyncTimeline)
            .into_raw()
            .deserialize()
            .expect("expected original sync room message event")
    }

    pub fn into_raw_sync(self) -> Raw<AnySyncTimelineEvent> {
        self.format(EventFormat::SyncTimeline).into_raw()
    }

    pub fn into_raw_sync_state(self) -> Raw<AnySyncStateEvent> {
        self.format(EventFormat::SyncTimeline).into_raw()
    }

    pub fn into_event(self) -> TimelineEvent {
        TimelineEvent::from_plaintext(self.into_raw_sync())
    }
}

impl EventBuilder<RoomEncryptedEventContent> {
    /// Turn this event into a [`TimelineEvent`] representing a decryption
    /// failure
    pub fn into_utd_sync_timeline_event(self) -> TimelineEvent {
        let session_id = as_variant!(&self.content.scheme, EncryptedEventScheme::MegolmV1AesSha2)
            .map(|content| content.session_id.clone());

        TimelineEvent::from_utd(
            self.into(),
            UnableToDecryptInfo {
                session_id,
                reason: UnableToDecryptReason::MissingMegolmSession { withheld_code: None },
            },
        )
    }
}

impl EventBuilder<RoomMessageEventContent> {
    /// Adds a reply relation to the current event.
    pub fn reply_to(mut self, event_id: &EventId) -> Self {
        self.content.relates_to =
            Some(Relation::Reply { in_reply_to: InReplyTo::new(event_id.to_owned()) });
        self
    }

    /// Adds a thread relation to the root event, setting the reply fallback to
    /// the latest in-thread event.
    pub fn in_thread(mut self, root: &EventId, latest_thread_event: &EventId) -> Self {
        self.content.relates_to =
            Some(Relation::Thread(Thread::plain(root.to_owned(), latest_thread_event.to_owned())));
        self
    }

    /// Adds a thread relation to the root event, that's a non-fallback reply to
    /// another thread event.
    pub fn in_thread_reply(mut self, root: &EventId, replied_to: &EventId) -> Self {
        self.content.relates_to =
            Some(Relation::Thread(Thread::reply(root.to_owned(), replied_to.to_owned())));
        self
    }

    /// Adds the given mentions to the current event.
    pub fn mentions(mut self, mentions: Mentions) -> Self {
        self.content.mentions = Some(mentions);
        self
    }

    /// Adds a replacement relation to the current event, with the new content
    /// passed.
    pub fn edit(
        mut self,
        edited_event_id: &EventId,
        new_content: RoomMessageEventContentWithoutRelation,
    ) -> Self {
        self.content.relates_to =
            Some(Relation::Replacement(Replacement::new(edited_event_id.to_owned(), new_content)));
        self
    }

    /// Adds a caption to a media event.
    ///
    /// Will crash if the event isn't a media room message.
    pub fn caption(
        mut self,
        caption: Option<String>,
        formatted_caption: Option<FormattedBody>,
    ) -> Self {
        match &mut self.content.msgtype {
            MessageType::Image(image) => {
                let filename = image.filename().to_owned();
                if let Some(caption) = caption {
                    image.body = caption;
                    image.filename = Some(filename);
                } else {
                    image.body = filename;
                    image.filename = None;
                }
                image.formatted = formatted_caption;
            }

            MessageType::Audio(_) | MessageType::Video(_) | MessageType::File(_) => {
                unimplemented!();
            }

            _ => panic!("unexpected event type for a caption"),
        }

        self
    }
}

impl EventBuilder<UnstablePollStartEventContent> {
    /// Adds a reply relation to the current event.
    pub fn reply_to(mut self, event_id: &EventId) -> Self {
        if let UnstablePollStartEventContent::New(content) = &mut self.content {
            content.relates_to = Some(RelationWithoutReplacement::Reply {
                in_reply_to: InReplyTo::new(event_id.to_owned()),
            });
        }
        self
    }

    /// Adds a thread relation to the root event, setting the reply to
    /// event id as well.
    pub fn in_thread(mut self, root: &EventId, reply_to_event_id: &EventId) -> Self {
        let thread = Thread::reply(root.to_owned(), reply_to_event_id.to_owned());

        if let UnstablePollStartEventContent::New(content) = &mut self.content {
            content.relates_to = Some(RelationWithoutReplacement::Thread(thread));
        }
        self
    }
}

impl EventBuilder<RoomCreateEventContent> {
    /// Define the predecessor fields.
    pub fn predecessor(mut self, room_id: &RoomId) -> Self {
        self.content.predecessor = Some(PreviousRoom::new(room_id.to_owned()));
        self
    }

    /// Erase the predecessor if any.
    pub fn no_predecessor(mut self) -> Self {
        self.content.predecessor = None;
        self
    }

    /// Sets the `m.room.create` `type` field to `m.space`.
    pub fn with_space_type(mut self) -> Self {
        self.content.room_type = Some(RoomType::Space);
        self
    }
}

impl EventBuilder<StickerEventContent> {
    /// Add reply [`Thread`] relation to root event and set replied-to event id.
    pub fn reply_thread(mut self, root: &EventId, reply_to_event: &EventId) -> Self {
        self.content.relates_to =
            Some(Relation::Thread(Thread::reply(root.to_owned(), reply_to_event.to_owned())));
        self
    }
}

impl<E: StaticEventContent<IsPrefix = False>> From<EventBuilder<E>> for Raw<AnySyncTimelineEvent>
where
    E: Serialize,
{
    fn from(val: EventBuilder<E>) -> Self {
        val.into_raw_sync()
    }
}

impl<E: StaticEventContent<IsPrefix = False>> From<EventBuilder<E>> for AnySyncTimelineEvent
where
    E: Serialize,
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<AnySyncTimelineEvent>::from(val).deserialize().expect("expected sync timeline event")
    }
}

impl<E: StaticEventContent<IsPrefix = False>> From<EventBuilder<E>> for Raw<AnyTimelineEvent>
where
    E: Serialize,
{
    fn from(val: EventBuilder<E>) -> Self {
        val.into_raw_timeline()
    }
}

impl<E: StaticEventContent<IsPrefix = False>> From<EventBuilder<E>> for AnyTimelineEvent
where
    E: Serialize,
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<AnyTimelineEvent>::from(val).deserialize().expect("expected timeline event")
    }
}

impl<E: StaticEventContent<IsPrefix = False>> From<EventBuilder<E>>
    for Raw<AnyGlobalAccountDataEvent>
where
    E: Serialize,
{
    fn from(val: EventBuilder<E>) -> Self {
        val.format(EventFormat::GlobalAccountData).into_raw()
    }
}

impl<E: StaticEventContent<IsPrefix = False>> From<EventBuilder<E>> for AnyGlobalAccountDataEvent
where
    E: Serialize,
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<AnyGlobalAccountDataEvent>::from(val)
            .deserialize()
            .expect("expected global account data")
    }
}

impl<E: StaticEventContent<IsPrefix = False>> From<EventBuilder<E>> for TimelineEvent
where
    E: Serialize,
{
    fn from(val: EventBuilder<E>) -> Self {
        val.into_event()
    }
}

impl<E: StaticEventContent<IsPrefix = False> + StateEventContent> From<EventBuilder<E>>
    for Raw<AnySyncStateEvent>
{
    fn from(val: EventBuilder<E>) -> Self {
        val.format(EventFormat::SyncTimeline).into_raw()
    }
}

impl<E: StaticEventContent<IsPrefix = False> + StateEventContent> From<EventBuilder<E>>
    for AnySyncStateEvent
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<AnySyncStateEvent>::from(val).deserialize().expect("expected sync state")
    }
}

impl<E: StaticEventContent<IsPrefix = False> + StateEventContent> From<EventBuilder<E>>
    for Raw<SyncStateEvent<E>>
where
    E: StaticStateEventContent + RedactContent,
    E::Redacted: RedactedStateEventContent,
{
    fn from(val: EventBuilder<E>) -> Self {
        val.format(EventFormat::SyncTimeline).into_raw()
    }
}

impl<E: StaticEventContent<IsPrefix = False> + StateEventContent> From<EventBuilder<E>>
    for SyncStateEvent<E>
where
    E: StaticStateEventContent + RedactContent + EventContentFromType,
    E::Redacted: RedactedStateEventContent<StateKey = <E as StateEventContent>::StateKey>
        + EventContentFromType,
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<SyncStateEvent<E>>::from(val).deserialize().expect("expected sync state")
    }
}

impl<E: StaticEventContent<IsPrefix = False> + StateEventContent> From<EventBuilder<E>>
    for Raw<AnyStateEvent>
{
    fn from(val: EventBuilder<E>) -> Self {
        val.into_raw()
    }
}

impl<E: StaticEventContent<IsPrefix = False> + StateEventContent> From<EventBuilder<E>>
    for AnyStateEvent
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<AnyStateEvent>::from(val).deserialize().expect("expected state")
    }
}

impl<E: StaticEventContent<IsPrefix = False> + StateEventContent> From<EventBuilder<E>>
    for Raw<StateEvent<E>>
where
    E: StaticStateEventContent + RedactContent,
    E::Redacted: RedactedStateEventContent,
{
    fn from(val: EventBuilder<E>) -> Self {
        val.into_raw()
    }
}

impl<E: StaticEventContent<IsPrefix = False> + StateEventContent> From<EventBuilder<E>>
    for StateEvent<E>
where
    E: StaticStateEventContent + RedactContent + EventContentFromType,
    E::Redacted: RedactedStateEventContent<StateKey = <E as StateEventContent>::StateKey>
        + EventContentFromType,
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<StateEvent<E>>::from(val).deserialize().expect("expected state")
    }
}

impl<E: StaticEventContent<IsPrefix = False> + StateEventContent> From<EventBuilder<E>>
    for Raw<AnyStrippedStateEvent>
{
    fn from(val: EventBuilder<E>) -> Self {
        val.format(EventFormat::StrippedState).into_raw()
    }
}

impl<E: StaticEventContent<IsPrefix = False> + StateEventContent> From<EventBuilder<E>>
    for AnyStrippedStateEvent
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<AnyStrippedStateEvent>::from(val).deserialize().expect("expected stripped state")
    }
}

impl<E: StaticEventContent<IsPrefix = False> + StateEventContent> From<EventBuilder<E>>
    for Raw<StrippedStateEvent<E::PossiblyRedacted>>
where
    E: StaticStateEventContent,
{
    fn from(val: EventBuilder<E>) -> Self {
        val.format(EventFormat::StrippedState).into_raw()
    }
}

impl<E: StaticEventContent<IsPrefix = False> + StateEventContent> From<EventBuilder<E>>
    for StrippedStateEvent<E::PossiblyRedacted>
where
    E: StaticStateEventContent,
    E::PossiblyRedacted: PossiblyRedactedStateEventContent + EventContentFromType,
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<StrippedStateEvent<E::PossiblyRedacted>>::from(val)
            .deserialize()
            .expect("expected stripped state")
    }
}

impl<E: StaticEventContent<IsPrefix = False> + EphemeralRoomEventContent> From<EventBuilder<E>>
    for Raw<AnySyncEphemeralRoomEvent>
{
    fn from(val: EventBuilder<E>) -> Self {
        val.format(EventFormat::Ephemeral).into_raw()
    }
}

impl<E: StaticEventContent<IsPrefix = False> + EphemeralRoomEventContent> From<EventBuilder<E>>
    for AnySyncEphemeralRoomEvent
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<AnySyncEphemeralRoomEvent>::from(val).deserialize().expect("expected ephemeral")
    }
}

impl<E: StaticEventContent<IsPrefix = False> + MessageLikeEventContent> From<EventBuilder<E>>
    for Raw<AnySyncMessageLikeEvent>
{
    fn from(val: EventBuilder<E>) -> Self {
        val.format(EventFormat::SyncTimeline).into_raw()
    }
}

impl<E: StaticEventContent<IsPrefix = False> + MessageLikeEventContent> From<EventBuilder<E>>
    for AnySyncMessageLikeEvent
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<AnySyncMessageLikeEvent>::from(val).deserialize().expect("expected sync message-like")
    }
}

impl<E: StaticEventContent<IsPrefix = False> + MessageLikeEventContent> From<EventBuilder<E>>
    for Raw<SyncMessageLikeEvent<E>>
where
    E: RedactContent,
    E::Redacted: RedactedMessageLikeEventContent,
{
    fn from(val: EventBuilder<E>) -> Self {
        val.format(EventFormat::SyncTimeline).into_raw()
    }
}

impl<E: StaticEventContent<IsPrefix = False> + MessageLikeEventContent> From<EventBuilder<E>>
    for SyncMessageLikeEvent<E>
where
    E: RedactContent + EventContentFromType,
    E::Redacted: RedactedMessageLikeEventContent + EventContentFromType,
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<SyncMessageLikeEvent<E>>::from(val).deserialize().expect("expected sync message-like")
    }
}

impl<E: StaticEventContent<IsPrefix = False> + MessageLikeEventContent> From<EventBuilder<E>>
    for Raw<AnyMessageLikeEvent>
{
    fn from(val: EventBuilder<E>) -> Self {
        val.into_raw()
    }
}

impl<E: StaticEventContent<IsPrefix = False> + MessageLikeEventContent> From<EventBuilder<E>>
    for AnyMessageLikeEvent
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<AnyMessageLikeEvent>::from(val).deserialize().expect("expected message-like")
    }
}

impl<E: StaticEventContent<IsPrefix = False> + MessageLikeEventContent> From<EventBuilder<E>>
    for Raw<MessageLikeEvent<E>>
where
    E: RedactContent,
    E::Redacted: RedactedMessageLikeEventContent,
{
    fn from(val: EventBuilder<E>) -> Self {
        val.into_raw()
    }
}

impl<E: StaticEventContent<IsPrefix = False> + MessageLikeEventContent> From<EventBuilder<E>>
    for MessageLikeEvent<E>
where
    E: RedactContent + EventContentFromType,
    E::Redacted: RedactedMessageLikeEventContent + EventContentFromType,
{
    fn from(val: EventBuilder<E>) -> Self {
        Raw::<MessageLikeEvent<E>>::from(val).deserialize().expect("expected message-like")
    }
}

#[derive(Debug, Default)]
pub struct EventFactory {
    next_ts: AtomicU64,
    sender: Option<OwnedUserId>,
    room: Option<OwnedRoomId>,
}

impl EventFactory {
    pub fn new() -> Self {
        Self { next_ts: AtomicU64::new(0), sender: None, room: None }
    }

    pub fn room(mut self, room_id: &RoomId) -> Self {
        self.room = Some(room_id.to_owned());
        self
    }

    pub fn sender(mut self, sender: &UserId) -> Self {
        self.sender = Some(sender.to_owned());
        self
    }

    pub fn server_ts(self, ts: u64) -> Self {
        self.next_ts.store(ts, SeqCst);
        self
    }

    fn next_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch(
            self.next_ts
                .fetch_add(1, SeqCst)
                .try_into()
                .expect("server timestamp should fit in js_int::UInt"),
        )
    }

    /// Create an event from any event content.
    pub fn event<E: StaticEventContent<IsPrefix = False>>(&self, content: E) -> EventBuilder<E> {
        EventBuilder {
            format: EventFormat::Timeline,
            sender: self.sender.clone(),
            room: self.room.clone(),
            server_ts: self.next_server_ts(),
            event_id: None,
            no_event_id: false,
            redacts: None,
            content,
            unsigned: None,
            state_key: None,
        }
    }

    /// Create a new plain text `m.room.message`.
    pub fn text_msg(&self, content: impl Into<String>) -> EventBuilder<RoomMessageEventContent> {
        self.event(RoomMessageEventContent::text_plain(content.into()))
    }

    /// Create a new plain emote `m.room.message`.
    pub fn emote(&self, content: impl Into<String>) -> EventBuilder<RoomMessageEventContent> {
        self.event(RoomMessageEventContent::emote_plain(content.into()))
    }

    /// Create a new `m.room.encrypted` event using the `m.megolm.v1.aes-sha2`
    /// algorithm.
    pub fn encrypted(
        &self,
        ciphertext: impl Into<String>,
        sender_key: impl Into<String>,
        device_id: impl Into<OwnedDeviceId>,
        session_id: impl Into<String>,
    ) -> EventBuilder<RoomEncryptedEventContent> {
        self.event(RoomEncryptedEventContent::new(
            EncryptedEventScheme::MegolmV1AesSha2(
                MegolmV1AesSha2ContentInit {
                    ciphertext: ciphertext.into(),
                    sender_key: sender_key.into(),
                    device_id: device_id.into(),
                    session_id: session_id.into(),
                }
                .into(),
            ),
            None,
        ))
    }

    /// Create a new `m.room.member` event for the given member.
    ///
    /// The given member will be used as the `sender` as well as the `state_key`
    /// of the `m.room.member` event, unless the `sender` was already using
    /// [`EventFactory::sender()`], in that case only the state key will be
    /// set to the given `member`.
    ///
    /// The `membership` field of the content is set to
    /// [`MembershipState::Join`].
    ///
    /// ```
    /// use matrix_sdk_test::event_factory::EventFactory;
    /// use ruma::{
    ///     events::{
    ///         SyncStateEvent,
    ///         room::member::{MembershipState, RoomMemberEventContent},
    ///     },
    ///     room_id,
    ///     serde::Raw,
    ///     user_id,
    /// };
    ///
    /// let factory = EventFactory::new().room(room_id!("!test:localhost"));
    ///
    /// let event: Raw<SyncStateEvent<RoomMemberEventContent>> = factory
    ///     .member(user_id!("@alice:localhost"))
    ///     .display_name("Alice")
    ///     .into_raw();
    /// ```
    pub fn member(&self, member: &UserId) -> EventBuilder<RoomMemberEventContent> {
        let mut event = self.event(RoomMemberEventContent::new(MembershipState::Join));

        if self.sender.is_some() {
            event.sender = self.sender.clone();
        } else {
            event.sender = Some(member.to_owned());
        }

        event.state_key = Some(member.to_string());

        event
    }

    /// Create a tombstone state event for the room.
    pub fn room_tombstone(
        &self,
        body: impl Into<String>,
        replacement: &RoomId,
    ) -> EventBuilder<RoomTombstoneEventContent> {
        let mut event =
            self.event(RoomTombstoneEventContent::new(body.into(), replacement.to_owned()));
        event.state_key = Some("".to_owned());
        event
    }

    /// Create a state event for the topic.
    pub fn room_topic(&self, topic: impl Into<String>) -> EventBuilder<RoomTopicEventContent> {
        let mut event = self.event(RoomTopicEventContent::new(topic.into()));
        // The state key is empty for a room topic state event.
        event.state_key = Some("".to_owned());
        event
    }

    /// Create a state event for the room name.
    pub fn room_name(&self, name: impl Into<String>) -> EventBuilder<RoomNameEventContent> {
        let mut event = self.event(RoomNameEventContent::new(name.into()));
        // The state key is empty for a room name state event.
        event.state_key = Some("".to_owned());
        event
    }

    /// Create an empty state event for the room avatar.
    pub fn room_avatar(&self) -> EventBuilder<RoomAvatarEventContent> {
        let mut event = self.event(RoomAvatarEventContent::new());
        // The state key is empty for a room avatar state event.
        event.state_key = Some("".to_owned());
        event
    }

    /// Create a new `m.member_hints` event with the given service members.
    ///
    /// ```
    /// use std::collections::BTreeSet;
    ///
    /// use matrix_sdk_test::event_factory::EventFactory;
    /// use ruma::{
    ///     events::{SyncStateEvent, member_hints::MemberHintsEventContent},
    ///     owned_user_id, room_id,
    ///     serde::Raw,
    ///     user_id,
    /// };
    ///
    /// let factory = EventFactory::new().room(room_id!("!test:localhost"));
    ///
    /// let event: Raw<SyncStateEvent<MemberHintsEventContent>> = factory
    ///     .member_hints(BTreeSet::from([owned_user_id!("@alice:localhost")]))
    ///     .sender(user_id!("@alice:localhost"))
    ///     .into_raw();
    /// ```
    pub fn member_hints(
        &self,
        service_members: BTreeSet<OwnedUserId>,
    ) -> EventBuilder<MemberHintsEventContent> {
        // The `m.member_hints` event always has an empty state key, so let's set it.
        self.event(MemberHintsEventContent::new(service_members)).state_key("")
    }

    /// Create a new plain/html `m.room.message`.
    pub fn text_html(
        &self,
        plain: impl Into<String>,
        html: impl Into<String>,
    ) -> EventBuilder<RoomMessageEventContent> {
        self.event(RoomMessageEventContent::text_html(plain, html))
    }

    /// Create a new plain notice `m.room.message`.
    pub fn notice(&self, content: impl Into<String>) -> EventBuilder<RoomMessageEventContent> {
        self.event(RoomMessageEventContent::notice_plain(content))
    }

    /// Add a reaction to an event.
    pub fn reaction(
        &self,
        event_id: &EventId,
        annotation: impl Into<String>,
    ) -> EventBuilder<ReactionEventContent> {
        self.event(ReactionEventContent::new(Annotation::new(
            event_id.to_owned(),
            annotation.into(),
        )))
    }

    /// Create a live redaction for the given event id.
    ///
    /// Note: this is not a redacted event, but a redaction event, that will
    /// cause another event to be redacted.
    pub fn redaction(&self, event_id: &EventId) -> EventBuilder<RoomRedactionEventContent> {
        let mut builder = self.event(RoomRedactionEventContent::new_v11(event_id.to_owned()));
        builder.redacts = Some(event_id.to_owned());
        builder
    }

    /// Create a redacted event, with extra information in the unsigned section
    /// about the redaction itself.
    pub fn redacted<T: StaticEventContent<IsPrefix = False> + RedactedMessageLikeEventContent>(
        &self,
        redacter: &UserId,
        content: T,
    ) -> EventBuilder<T> {
        let mut builder = self.event(content);

        let redacted_because = RedactedBecause {
            content: RoomRedactionEventContent::default(),
            event_id: EventId::new(server_name!("dummy.server")),
            sender: redacter.to_owned(),
            origin_server_ts: self.next_server_ts(),
        };
        builder.unsigned.get_or_insert_with(Default::default).redacted_because =
            Some(redacted_because);

        builder
    }

    /// Create a redacted state event, with extra information in the unsigned
    /// section about the redaction itself.
    pub fn redacted_state<T: StaticEventContent<IsPrefix = False> + RedactedStateEventContent>(
        &self,
        redacter: &UserId,
        state_key: impl Into<String>,
        content: T,
    ) -> EventBuilder<T> {
        let mut builder = self.event(content);

        let redacted_because = RedactedBecause {
            content: RoomRedactionEventContent::default(),
            event_id: EventId::new(server_name!("dummy.server")),
            sender: redacter.to_owned(),
            origin_server_ts: self.next_server_ts(),
        };
        builder.unsigned.get_or_insert_with(Default::default).redacted_because =
            Some(redacted_because);
        builder.state_key = Some(state_key.into());

        builder
    }

    /// Create a poll start event given a text, the question and the possible
    /// answers.
    pub fn poll_start(
        &self,
        fallback_text: impl Into<String>,
        poll_question: impl Into<String>,
        answers: Vec<impl Into<String>>,
    ) -> EventBuilder<UnstablePollStartEventContent> {
        // PollAnswers 'constructor' is not public, so we need to deserialize them
        let answers: Vec<UnstablePollAnswer> = answers
            .into_iter()
            .enumerate()
            .map(|(idx, answer)| UnstablePollAnswer::new(idx.to_string(), answer))
            .collect();
        let poll_answers = answers.try_into().unwrap();
        let poll_start_content =
            UnstablePollStartEventContent::New(NewUnstablePollStartEventContent::plain_text(
                fallback_text,
                UnstablePollStartContentBlock::new(poll_question, poll_answers),
            ));
        self.event(poll_start_content)
    }

    /// Create a poll edit event given the new question and possible answers.
    pub fn poll_edit(
        &self,
        edited_event_id: &EventId,
        poll_question: impl Into<String>,
        answers: Vec<impl Into<String>>,
    ) -> EventBuilder<ReplacementUnstablePollStartEventContent> {
        // PollAnswers 'constructor' is not public, so we need to deserialize them
        let answers: Vec<UnstablePollAnswer> = answers
            .into_iter()
            .enumerate()
            .map(|(idx, answer)| UnstablePollAnswer::new(idx.to_string(), answer))
            .collect();
        let poll_answers = answers.try_into().unwrap();
        let poll_start_content_block =
            UnstablePollStartContentBlock::new(poll_question, poll_answers);
        self.event(ReplacementUnstablePollStartEventContent::new(
            poll_start_content_block,
            edited_event_id.to_owned(),
        ))
    }

    /// Create a poll response with the given answer id and the associated poll
    /// start event id.
    pub fn poll_response(
        &self,
        answers: Vec<impl Into<String>>,
        poll_start_id: &EventId,
    ) -> EventBuilder<UnstablePollResponseEventContent> {
        self.event(UnstablePollResponseEventContent::new(
            answers.into_iter().map(Into::into).collect(),
            poll_start_id.to_owned(),
        ))
    }

    /// Create a poll response with the given text and the associated poll start
    /// event id.
    pub fn poll_end(
        &self,
        content: impl Into<String>,
        poll_start_id: &EventId,
    ) -> EventBuilder<UnstablePollEndEventContent> {
        self.event(UnstablePollEndEventContent::new(content.into(), poll_start_id.to_owned()))
    }

    /// Creates a plain (unencrypted) image event content referencing the given
    /// MXC ID.
    pub fn image(
        &self,
        filename: String,
        url: OwnedMxcUri,
    ) -> EventBuilder<RoomMessageEventContent> {
        let image_event_content = ImageMessageEventContent::plain(filename, url);
        self.event(RoomMessageEventContent::new(MessageType::Image(image_event_content)))
    }

    /// Create a gallery event containing a single plain (unencrypted) image
    /// referencing the given MXC ID.
    pub fn gallery(
        &self,
        body: String,
        filename: String,
        url: OwnedMxcUri,
    ) -> EventBuilder<RoomMessageEventContent> {
        let gallery_event_content = GalleryMessageEventContent::new(
            body,
            None,
            vec![GalleryItemType::Image(ImageMessageEventContent::plain(filename, url))],
        );
        self.event(RoomMessageEventContent::new(MessageType::Gallery(gallery_event_content)))
    }

    /// Create a typing notification event.
    pub fn typing(&self, user_ids: Vec<&UserId>) -> EventBuilder<TypingEventContent> {
        self.event(TypingEventContent::new(user_ids.into_iter().map(ToOwned::to_owned).collect()))
            .format(EventFormat::Ephemeral)
    }

    /// Create a read receipt event.
    pub fn read_receipts(&self) -> ReadReceiptBuilder<'_> {
        ReadReceiptBuilder { factory: self, content: ReceiptEventContent(Default::default()) }
    }

    /// Create a new `m.room.create` event.
    pub fn create(
        &self,
        creator_user_id: &UserId,
        room_version: RoomVersionId,
    ) -> EventBuilder<RoomCreateEventContent> {
        let mut event = self.event(RoomCreateEventContent::new_v1(creator_user_id.to_owned()));
        event.content.room_version = room_version;

        if self.sender.is_some() {
            event.sender = self.sender.clone();
        } else {
            event.sender = Some(creator_user_id.to_owned());
        }

        event.state_key = Some("".to_owned());

        event
    }

    /// Create a new `m.room.power_levels` event.
    pub fn power_levels(
        &self,
        map: &mut BTreeMap<OwnedUserId, Int>,
    ) -> EventBuilder<RoomPowerLevelsEventContent> {
        let mut event = RoomPowerLevelsEventContent::new(&AuthorizationRules::V1);
        event.users.append(map);
        self.event(event)
    }

    /// Create a new `m.room.server_acl` event.
    pub fn server_acl(
        &self,
        allow_ip_literals: bool,
        allow: Vec<String>,
        deny: Vec<String>,
    ) -> EventBuilder<RoomServerAclEventContent> {
        self.event(RoomServerAclEventContent::new(allow_ip_literals, allow, deny))
    }

    /// Create a new `m.room.canonical_alias` event.
    pub fn canonical_alias(
        &self,
        alias: Option<OwnedRoomAliasId>,
        alt_aliases: Vec<OwnedRoomAliasId>,
    ) -> EventBuilder<RoomCanonicalAliasEventContent> {
        let mut event = RoomCanonicalAliasEventContent::new();
        event.alias = alias;
        event.alt_aliases = alt_aliases;
        self.event(event)
    }

    /// Create a new `org.matrix.msc3672.beacon` event.
    ///
    /// ```
    /// use matrix_sdk_test::event_factory::EventFactory;
    /// use ruma::{
    ///     MilliSecondsSinceUnixEpoch,
    ///     events::{MessageLikeEvent, beacon::BeaconEventContent},
    ///     owned_event_id, room_id,
    ///     serde::Raw,
    ///     user_id,
    /// };
    ///
    /// let factory = EventFactory::new().room(room_id!("!test:localhost"));
    ///
    /// let event: Raw<MessageLikeEvent<BeaconEventContent>> = factory
    ///     .beacon(
    ///         owned_event_id!("$123456789abc:localhost"),
    ///         10.1,
    ///         15.2,
    ///         5,
    ///         Some(MilliSecondsSinceUnixEpoch(1000u32.into())),
    ///     )
    ///     .sender(user_id!("@alice:localhost"))
    ///     .into_raw();
    /// ```
    pub fn beacon(
        &self,
        beacon_info_event_id: OwnedEventId,
        latitude: f64,
        longitude: f64,
        uncertainty: u32,
        ts: Option<MilliSecondsSinceUnixEpoch>,
    ) -> EventBuilder<BeaconEventContent> {
        let geo_uri = format!("geo:{latitude},{longitude};u={uncertainty}");
        self.event(BeaconEventContent::new(beacon_info_event_id, geo_uri, ts))
    }

    /// Create a new `m.sticker` event.
    pub fn sticker(
        &self,
        body: impl Into<String>,
        info: ImageInfo,
        url: OwnedMxcUri,
    ) -> EventBuilder<StickerEventContent> {
        self.event(StickerEventContent::new(body.into(), info, url))
    }

    /// Create a new `m.call.invite` event.
    pub fn call_invite(
        &self,
        call_id: OwnedVoipId,
        lifetime: UInt,
        offer: SessionDescription,
        version: VoipVersionId,
    ) -> EventBuilder<CallInviteEventContent> {
        self.event(CallInviteEventContent::new(call_id, lifetime, offer, version))
    }

    /// Create a new `m.rtc.notification` event.
    pub fn rtc_notification(
        &self,
        notification_type: NotificationType,
    ) -> EventBuilder<RtcNotificationEventContent> {
        self.event(RtcNotificationEventContent::new(
            MilliSecondsSinceUnixEpoch::now(),
            Duration::new(30, 0),
            notification_type,
        ))
    }

    // Creates a new `org.matrix.msc4310.rtc.decline` event.
    pub fn call_decline(
        &self,
        notification_event_id: &EventId,
    ) -> EventBuilder<RtcDeclineEventContent> {
        self.event(RtcDeclineEventContent::new(notification_event_id))
    }

    /// Create a new `m.direct` global account data event.
    pub fn direct(&self) -> EventBuilder<DirectEventContent> {
        self.global_account_data(DirectEventContent::default())
    }

    /// Create a new `m.ignored_user_list` global account data event.
    pub fn ignored_user_list(
        &self,
        users: impl IntoIterator<Item = OwnedUserId>,
    ) -> EventBuilder<IgnoredUserListEventContent> {
        self.global_account_data(IgnoredUserListEventContent::users(users))
    }

    /// Create a new `m.push_rules` global account data event.
    pub fn push_rules(&self, rules: Ruleset) -> EventBuilder<PushRulesEventContent> {
        self.global_account_data(PushRulesEventContent::new(rules))
    }

    /// Create a new `m.space.child` state event.
    pub fn space_child(
        &self,
        parent: OwnedRoomId,
        child: OwnedRoomId,
    ) -> EventBuilder<SpaceChildEventContent> {
        let mut event = self.event(SpaceChildEventContent::new(vec![]));
        event.room = Some(parent);
        event.state_key = Some(child.to_string());
        event
    }

    /// Create a new `m.space.parent` state event.
    pub fn space_parent(
        &self,
        parent: OwnedRoomId,
        child: OwnedRoomId,
    ) -> EventBuilder<SpaceParentEventContent> {
        let mut event = self.event(SpaceParentEventContent::new(vec![]));
        event.state_key = Some(parent.to_string());
        event.room = Some(child);
        event
    }

    /// Create a new `rs.matrix-sdk.custom.test` custom event
    pub fn custom_message_like_event(&self) -> EventBuilder<CustomMessageLikeEventContent> {
        self.event(CustomMessageLikeEventContent)
    }

    /// Set the next server timestamp.
    ///
    /// Timestamps will continue to increase by 1 (millisecond) from that value.
    pub fn set_next_ts(&self, value: u64) {
        self.next_ts.store(value, SeqCst);
    }

    /// Create a new global account data event of the given `C` content type.
    pub fn global_account_data<C>(&self, content: C) -> EventBuilder<C>
    where
        C: GlobalAccountDataEventContent + StaticEventContent<IsPrefix = False>,
    {
        self.event(content).format(EventFormat::GlobalAccountData)
    }
}

impl EventBuilder<DirectEventContent> {
    /// Add a user/room pair to the `m.direct` event.
    pub fn add_user(mut self, user_id: OwnedDirectUserIdentifier, room_id: &RoomId) -> Self {
        self.content.0.entry(user_id).or_default().push(room_id.to_owned());
        self
    }
}

impl EventBuilder<RoomMemberEventContent> {
    /// Set the `membership` of the `m.room.member` event to the given
    /// [`MembershipState`].
    ///
    /// The default is [`MembershipState::Join`].
    pub fn membership(mut self, state: MembershipState) -> Self {
        self.content.membership = state;
        self
    }

    /// Set that the sender of this event invited the user passed as a parameter
    /// here.
    pub fn invited(mut self, invited_user: &UserId) -> Self {
        assert_ne!(
            self.sender.as_deref().unwrap(),
            invited_user,
            "invited user and sender can't be the same person"
        );
        self.content.membership = MembershipState::Invite;
        self.state_key = Some(invited_user.to_string());
        self
    }

    /// Set that the sender of this event kicked the user passed as a parameter
    /// here.
    pub fn kicked(mut self, kicked_user: &UserId) -> Self {
        assert_ne!(
            self.sender.as_deref().unwrap(),
            kicked_user,
            "kicked user and sender can't be the same person, otherwise it's just a Leave"
        );
        self.content.membership = MembershipState::Leave;
        self.state_key = Some(kicked_user.to_string());
        self
    }

    /// Set that the sender of this event banned the user passed as a parameter
    /// here.
    pub fn banned(mut self, banned_user: &UserId) -> Self {
        assert_ne!(
            self.sender.as_deref().unwrap(),
            banned_user,
            "a user can't ban itself" // hopefully
        );
        self.content.membership = MembershipState::Ban;
        self.state_key = Some(banned_user.to_string());
        self
    }

    /// Set the display name of the `m.room.member` event.
    pub fn display_name(mut self, display_name: impl Into<String>) -> Self {
        self.content.displayname = Some(display_name.into());
        self
    }

    /// Set the avatar URL of the `m.room.member` event.
    pub fn avatar_url(mut self, url: &MxcUri) -> Self {
        self.content.avatar_url = Some(url.to_owned());
        self
    }

    /// Set the reason field of the `m.room.member` event.
    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.content.reason = Some(reason.into());
        self
    }

    /// Set the previous membership state (in the unsigned section).
    pub fn previous(mut self, previous: impl Into<PreviousMembership>) -> Self {
        let previous = previous.into();

        let mut prev_content = RoomMemberEventContent::new(previous.state);
        if let Some(avatar_url) = previous.avatar_url {
            prev_content.avatar_url = Some(avatar_url);
        }
        if let Some(display_name) = previous.display_name {
            prev_content.displayname = Some(display_name);
        }

        self.unsigned.get_or_insert_with(Default::default).prev_content = Some(prev_content);
        self
    }
}

impl EventBuilder<RoomAvatarEventContent> {
    /// Defines the URL for the room avatar.
    pub fn url(mut self, url: &MxcUri) -> Self {
        self.content.url = Some(url.to_owned());
        self
    }

    /// Defines the image info for the avatar.
    pub fn info(mut self, image: avatar::ImageInfo) -> Self {
        self.content.info = Some(Box::new(image));
        self
    }
}

impl EventBuilder<RtcNotificationEventContent> {
    pub fn mentions(mut self, users: impl IntoIterator<Item = OwnedUserId>) -> Self {
        self.content.mentions = Some(Mentions::with_user_ids(users));
        self
    }

    pub fn relates_to_membership_state_event(mut self, event_id: OwnedEventId) -> Self {
        self.content.relates_to = Some(Reference::new(event_id));
        self
    }

    pub fn lifetime(mut self, time_in_seconds: u64) -> Self {
        self.content.lifetime = Duration::from_secs(time_in_seconds);
        self
    }
}

pub struct ReadReceiptBuilder<'a> {
    factory: &'a EventFactory,
    content: ReceiptEventContent,
}

impl ReadReceiptBuilder<'_> {
    /// Add a single read receipt to the event.
    pub fn add(
        self,
        event_id: &EventId,
        user_id: &UserId,
        tyype: ReceiptType,
        thread: ReceiptThread,
    ) -> Self {
        let ts = self.factory.next_server_ts();
        self.add_with_timestamp(event_id, user_id, tyype, thread, Some(ts))
    }

    /// Add a single read receipt to the event, with an optional timestamp.
    pub fn add_with_timestamp(
        mut self,
        event_id: &EventId,
        user_id: &UserId,
        tyype: ReceiptType,
        thread: ReceiptThread,
        ts: Option<MilliSecondsSinceUnixEpoch>,
    ) -> Self {
        let by_event = self.content.0.entry(event_id.to_owned()).or_default();
        let by_type = by_event.entry(tyype).or_default();

        let mut receipt = Receipt::default();
        if let Some(ts) = ts {
            receipt.ts = Some(ts);
        }
        receipt.thread = thread;

        by_type.insert(user_id.to_owned(), receipt);
        self
    }

    /// Finalize the builder into the receipt event content.
    pub fn into_content(self) -> ReceiptEventContent {
        self.content
    }

    /// Finalize the builder into an event builder.
    pub fn into_event(self) -> EventBuilder<ReceiptEventContent> {
        self.factory.event(self.into_content()).format(EventFormat::Ephemeral)
    }
}

pub struct PreviousMembership {
    state: MembershipState,
    avatar_url: Option<OwnedMxcUri>,
    display_name: Option<String>,
}

impl PreviousMembership {
    pub fn new(state: MembershipState) -> Self {
        Self { state, avatar_url: None, display_name: None }
    }

    pub fn avatar_url(mut self, url: &MxcUri) -> Self {
        self.avatar_url = Some(url.to_owned());
        self
    }

    pub fn display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }
}

impl From<MembershipState> for PreviousMembership {
    fn from(state: MembershipState) -> Self {
        Self::new(state)
    }
}

#[derive(Clone, Default, Debug, Serialize, EventContent)]
#[ruma_event(type = "rs.matrix-sdk.custom.test", kind = MessageLike)]
pub struct CustomMessageLikeEventContent;
