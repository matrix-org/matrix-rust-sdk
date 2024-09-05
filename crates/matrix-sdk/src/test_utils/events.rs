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

use std::sync::atomic::{AtomicU64, Ordering::SeqCst};

use matrix_sdk_base::deserialized_responses::{SyncTimelineEvent, TimelineEvent};
use ruma::{
    events::{
        message::TextContentBlock,
        poll::{
            end::PollEndEventContent,
            response::{PollResponseEventContent, SelectionsContentBlock},
            start::{PollAnswer, PollContentBlock, PollStartEventContent},
            unstable_start::{
                NewUnstablePollStartEventContent, UnstablePollAnswer,
                UnstablePollStartContentBlock, UnstablePollStartEventContent,
            },
        },
        reaction::ReactionEventContent,
        relation::{Annotation, InReplyTo, Replacement, Thread},
        room::{
            message::{Relation, RoomMessageEventContent, RoomMessageEventContentWithoutRelation},
            redaction::RoomRedactionEventContent,
        },
        AnySyncTimelineEvent, AnyTimelineEvent, EventContent,
    },
    serde::Raw,
    server_name, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId,
    OwnedTransactionId, OwnedUserId, RoomId, TransactionId, UserId,
};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Default, Serialize)]
struct Unsigned {
    transaction_id: Option<OwnedTransactionId>,
}

#[derive(Debug)]
pub struct EventBuilder<E: EventContent> {
    sender: Option<OwnedUserId>,
    room: Option<OwnedRoomId>,
    event_id: Option<OwnedEventId>,
    redacts: Option<OwnedEventId>,
    content: E,
    server_ts: MilliSecondsSinceUnixEpoch,
    unsigned: Option<Unsigned>,
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
        self
    }

    pub fn server_ts(mut self, ts: MilliSecondsSinceUnixEpoch) -> Self {
        self.server_ts = ts;
        self
    }

    pub fn unsigned_transaction_id(mut self, transaction_id: &TransactionId) -> Self {
        self.unsigned.get_or_insert_with(Default::default).transaction_id =
            Some(transaction_id.to_owned());
        self
    }

    #[inline(always)]
    fn construct_json<T>(self, requires_room: bool) -> Raw<T> {
        let event_id = self
            .event_id
            .or_else(|| {
                self.room.as_ref().map(|room_id| EventId::new(room_id.server_name().unwrap()))
            })
            .unwrap_or_else(|| EventId::new(server_name!("dummy.org")));

        let mut json = json!({
            "type": self.content.event_type(),
            "content": self.content,
            "event_id": event_id,
            "sender": self.sender.expect("we should have a sender user id at this point"),
            "origin_server_ts": self.server_ts,
        });

        let map = json.as_object_mut().unwrap();

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

        Raw::new(map).unwrap().cast()
    }

    pub fn into_raw_timeline(self) -> Raw<AnyTimelineEvent> {
        self.construct_json(true)
    }

    pub fn into_timeline(self) -> TimelineEvent {
        TimelineEvent::new(self.into_raw_timeline())
    }

    pub fn into_raw_sync(self) -> Raw<AnySyncTimelineEvent> {
        self.construct_json(false)
    }

    pub fn into_sync(self) -> SyncTimelineEvent {
        SyncTimelineEvent::new(self.into_raw_sync())
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

impl<E: EventContent> From<EventBuilder<E>> for SyncTimelineEvent
where
    E::EventType: Serialize,
{
    fn from(val: EventBuilder<E>) -> Self {
        val.into_sync()
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
            redacts: None,
            content,
            unsigned: None,
        }
    }

    /// Create a new plain text `m.room.message`.
    pub fn text_msg(&self, content: impl Into<String>) -> EventBuilder<RoomMessageEventContent> {
        self.event(RoomMessageEventContent::text_plain(content.into()))
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
        annotation: String,
    ) -> EventBuilder<ReactionEventContent> {
        self.event(ReactionEventContent::new(Annotation::new(event_id.to_owned(), annotation)))
    }

    /// Create a redaction for the given event id.
    pub fn redaction(&self, event_id: &EventId) -> EventBuilder<RoomRedactionEventContent> {
        let mut builder = self.event(RoomRedactionEventContent::new_v11(event_id.to_owned()));
        builder.redacts = Some(event_id.to_owned());
        builder
    }

    /// Create a poll start event given a text, the question and the possible
    /// answers.
    pub fn poll_start(
        &self,
        content: impl Into<String>,
        poll_question: impl Into<String>,
        answers: Vec<impl Into<String>>,
    ) -> EventBuilder<PollStartEventContent> {
        // PollAnswers 'constructor' is not public, so we need to deserialize them
        let answers: Vec<PollAnswer> = answers
            .into_iter()
            .enumerate()
            .map(|(idx, answer)| {
                PollAnswer::new(idx.to_string(), TextContentBlock::plain(answer.into()))
            })
            .collect();
        let poll_answers = answers.try_into().unwrap();
        let poll_start_content = PollStartEventContent::new(
            TextContentBlock::plain(content.into()),
            PollContentBlock::new(TextContentBlock::plain(poll_question.into()), poll_answers),
        );
        self.event(poll_start_content)
    }

    /// Create an unstable poll start event given a text, the question and the
    /// possible answers.
    pub fn unstable_poll_start(
        &self,
        content: impl Into<String>,
        poll_question: impl Into<String>,
        answers: Vec<impl Into<String>>,
    ) -> EventBuilder<UnstablePollStartEventContent> {
        // PollAnswers 'constructor' is not public, so we need to deserialize them
        let answers: Vec<UnstablePollAnswer> = answers
            .into_iter()
            .enumerate()
            .map(|(idx, answer)| UnstablePollAnswer::new(idx.to_string(), answer.into()))
            .collect();
        let poll_answers = answers.try_into().unwrap();
        let unstable_poll_start_content =
            UnstablePollStartEventContent::New(NewUnstablePollStartEventContent::plain_text(
                content,
                UnstablePollStartContentBlock::new(poll_question, poll_answers),
            ));
        self.event(unstable_poll_start_content)
    }

    /// Create a poll response with the given answer id and the associated poll
    /// start event id.
    pub fn poll_response(
        &self,
        answer_id: impl Into<String>,
        poll_start_id: &EventId,
    ) -> EventBuilder<PollResponseEventContent> {
        let selection_content: SelectionsContentBlock = vec![answer_id.into()].into();
        let poll_response_content =
            PollResponseEventContent::new(selection_content, poll_start_id.to_owned());
        self.event(poll_response_content)
    }

    /// Create a poll response with the given text and the associated poll start
    /// event id.
    pub fn poll_end(
        &self,
        content: impl Into<String>,
        poll_start_id: &EventId,
    ) -> EventBuilder<PollEndEventContent> {
        let poll_end_content = PollEndEventContent::new(
            TextContentBlock::plain(content.into()),
            poll_start_id.to_owned(),
        );
        self.event(poll_end_content)
    }

    /// Set the next server timestamp.
    ///
    /// Timestamps will continue to increase by 1 (millisecond) from that value.
    pub fn set_next_ts(&self, value: u64) {
        self.next_ts.store(value, SeqCst);
    }
}
