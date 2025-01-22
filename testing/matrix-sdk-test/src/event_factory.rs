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
    collections::BTreeSet,
    sync::atomic::{AtomicU64, Ordering::SeqCst},
};

use as_variant::as_variant;
use matrix_sdk_common::deserialized_responses::{
    TimelineEvent, UnableToDecryptInfo, UnableToDecryptReason,
};
use ruma::{
    events::{
        member_hints::MemberHintsEventContent,
        poll::{
            unstable_end::UnstablePollEndEventContent,
            unstable_response::UnstablePollResponseEventContent,
            unstable_start::{
                NewUnstablePollStartEventContent, ReplacementUnstablePollStartEventContent,
                UnstablePollAnswer, UnstablePollStartContentBlock, UnstablePollStartEventContent,
            },
        },
        reaction::ReactionEventContent,
        receipt::{Receipt, ReceiptEventContent, ReceiptThread, ReceiptType},
        relation::{Annotation, InReplyTo, Replacement, Thread},
        room::{
            avatar::{self, RoomAvatarEventContent},
            encrypted::{EncryptedEventScheme, RoomEncryptedEventContent},
            member::{MembershipState, RoomMemberEventContent},
            message::{
                FormattedBody, ImageMessageEventContent, MessageType, Relation,
                RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
            },
            name::RoomNameEventContent,
            redaction::RoomRedactionEventContent,
            topic::RoomTopicEventContent,
        },
        AnySyncTimelineEvent, AnyTimelineEvent, BundledMessageLikeRelations, EventContent,
        RedactedMessageLikeEventContent, RedactedStateEventContent,
    },
    serde::Raw,
    server_name, EventId, MilliSecondsSinceUnixEpoch, MxcUri, OwnedEventId, OwnedMxcUri,
    OwnedRoomId, OwnedTransactionId, OwnedUserId, RoomId, TransactionId, UInt, UserId,
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
struct Unsigned<C: EventContent> {
    #[serde(skip_serializing_if = "Option::is_none")]
    prev_content: Option<C>,

    #[serde(skip_serializing_if = "Option::is_none")]
    transaction_id: Option<OwnedTransactionId>,

    #[serde(rename = "m.relations", skip_serializing_if = "Option::is_none")]
    relations: Option<BundledMessageLikeRelations<Raw<AnySyncTimelineEvent>>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    redacted_because: Option<RedactedBecause>,
}

// rustc can't derive Default because C isn't marked as `Default` 🤔 oh well.
impl<C: EventContent> Default for Unsigned<C> {
    fn default() -> Self {
        Self { prev_content: None, transaction_id: None, relations: None, redacted_because: None }
    }
}

#[derive(Debug)]
pub struct EventBuilder<C: EventContent> {
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

impl<E: EventContent> EventBuilder<E>
where
    E::EventType: Serialize,
{
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

    /// Adds bundled relations to this event.
    ///
    /// Ideally, we'd type-check that an event passed as a relation is the same
    /// type as this one, but it's not trivial to do so because this builder
    /// is only generic on the event's *content*, not the event type itself;
    /// doing so would require many changes, and this is testing code after
    /// all.
    pub fn bundled_relations(
        mut self,
        relations: BundledMessageLikeRelations<Raw<AnySyncTimelineEvent>>,
    ) -> Self {
        self.unsigned.get_or_insert_with(Default::default).relations = Some(relations);
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

    #[inline(always)]
    fn construct_json(self, requires_room: bool) -> serde_json::Value {
        // Use the `sender` preferably, or resort to the `redacted_because` sender if
        // none has been set.
        let sender = self
            .sender
            .or_else(|| Some(self.unsigned.as_ref()?.redacted_because.as_ref()?.sender.clone()))
            .expect("we should have a sender user id at this point");

        let mut json = json!({
            "type": self.content.event_type(),
            "content": self.content,
            "sender": sender,
            "origin_server_ts": self.server_ts,
        });

        let map = json.as_object_mut().unwrap();

        let event_id = self
            .event_id
            .or_else(|| {
                self.room.as_ref().map(|room_id| EventId::new(room_id.server_name().unwrap()))
            })
            .or_else(|| (!self.no_event_id).then(|| EventId::new(server_name!("dummy.org"))));
        if let Some(event_id) = event_id {
            map.insert("event_id".to_owned(), json!(event_id));
        }

        if requires_room {
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
        Raw::new(&self.construct_json(true)).unwrap().cast()
    }

    pub fn into_raw_timeline(self) -> Raw<AnyTimelineEvent> {
        Raw::new(&self.construct_json(true)).unwrap().cast()
    }

    pub fn into_raw_sync(self) -> Raw<AnySyncTimelineEvent> {
        Raw::new(&self.construct_json(false)).unwrap().cast()
    }

    pub fn into_event(self) -> TimelineEvent {
        TimelineEvent::new(self.into_raw_sync())
    }
}

impl EventBuilder<RoomEncryptedEventContent> {
    /// Turn this event into a [`TimelineEvent`] representing a decryption
    /// failure
    pub fn into_utd_sync_timeline_event(self) -> TimelineEvent {
        let session_id = as_variant!(&self.content.scheme, EncryptedEventScheme::MegolmV1AesSha2)
            .map(|content| content.session_id.clone());

        TimelineEvent::new_utd_event(
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

    /// Adds a thread relation to the root event, setting the latest thread
    /// event id too.
    pub fn in_thread(mut self, root: &EventId, latest_thread_event: &EventId) -> Self {
        self.content.relates_to =
            Some(Relation::Thread(Thread::plain(root.to_owned(), latest_thread_event.to_owned())));
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

impl<E: EventContent> From<EventBuilder<E>> for Raw<AnySyncTimelineEvent>
where
    E::EventType: Serialize,
{
    fn from(val: EventBuilder<E>) -> Self {
        val.into_raw_sync()
    }
}

impl<E: EventContent> From<EventBuilder<E>> for Raw<AnyTimelineEvent>
where
    E::EventType: Serialize,
{
    fn from(val: EventBuilder<E>) -> Self {
        val.into_raw_timeline()
    }
}

impl<E: EventContent> From<EventBuilder<E>> for TimelineEvent
where
    E::EventType: Serialize,
{
    fn from(val: EventBuilder<E>) -> Self {
        val.into_event()
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

    fn next_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch(
            self.next_ts
                .fetch_add(1, SeqCst)
                .try_into()
                .expect("server timestamp should fit in js_int::UInt"),
        )
    }

    /// Create an event from any event content.
    pub fn event<E: EventContent>(&self, content: E) -> EventBuilder<E> {
        EventBuilder {
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
    ///         room::member::{MembershipState, RoomMemberEventContent},
    ///         SyncStateEvent,
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
    ///     events::{member_hints::MemberHintsEventContent, SyncStateEvent},
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
    pub fn redacted<T: RedactedMessageLikeEventContent>(
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
    pub fn redacted_state<T: RedactedStateEventContent>(
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
        content: impl Into<String>,
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
                content,
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

    /// Create a read receipt event.
    pub fn read_receipts(&self) -> ReadReceiptBuilder<'_> {
        ReadReceiptBuilder { factory: self, content: ReceiptEventContent(Default::default()) }
    }

    /// Set the next server timestamp.
    ///
    /// Timestamps will continue to increase by 1 (millisecond) from that value.
    pub fn set_next_ts(&self, value: u64) {
        self.next_ts.store(value, SeqCst);
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

pub struct ReadReceiptBuilder<'a> {
    factory: &'a EventFactory,
    content: ReceiptEventContent,
}

impl ReadReceiptBuilder<'_> {
    /// Add a single read receipt to the event.
    pub fn add(
        mut self,
        event_id: &EventId,
        user_id: &UserId,
        tyype: ReceiptType,
        thread: ReceiptThread,
    ) -> Self {
        let by_event = self.content.0.entry(event_id.to_owned()).or_default();
        let by_type = by_event.entry(tyype).or_default();

        let mut receipt = Receipt::new(self.factory.next_server_ts());
        receipt.thread = thread;

        by_type.insert(user_id.to_owned(), receipt);
        self
    }

    /// Finalize the builder into the receipt event content.
    pub fn build(self) -> ReceiptEventContent {
        self.content
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
