// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::fmt;

use futures_util::future::join_all;
use matrix_sdk_common::{debug::DebugStructExt as _, deserialized_responses::TimelineEvent};
use ruma::{
    OwnedEventId, RoomId, UInt,
    api::{
        Direction,
        client::{
            filter::RoomEventFilter,
            message::get_message_events,
            relations,
            threads::get_threads::{self, v1::IncludeThreads},
        },
    },
    assign,
    events::{AnyStateEvent, TimelineEventType, relation::RelationType},
    serde::Raw,
    uint,
};

use super::Room;
use crate::Result;

/// Options for [`messages`][super::Room::messages].
///
/// See that method and
/// <https://spec.matrix.org/v1.3/client-server-api/#get_matrixclientv3roomsroomidmessages>
/// for details.
#[non_exhaustive]
pub struct MessagesOptions {
    /// The token to start returning events from.
    ///
    /// This token can be obtained from a `prev_batch` token returned for each
    /// room from the sync API, or from a start or end token returned by a
    /// previous `messages` call.
    ///
    /// If `from` isn't provided the homeserver shall return a list of messages
    /// from the first or last (per the value of the dir parameter) visible
    /// event in the room history for the requesting user.
    pub from: Option<String>,

    /// The token to stop returning events at.
    ///
    /// This token can be obtained from a `prev_batch` token returned for each
    /// room by the sync API, or from a start or end token returned by a
    /// previous `messages` call.
    pub to: Option<String>,

    /// The direction to return events in.
    pub dir: Direction,

    /// The maximum number of events to return.
    ///
    /// Default: 10.
    pub limit: UInt,

    /// A [`RoomEventFilter`] to filter returned events with.
    pub filter: RoomEventFilter,
}

impl MessagesOptions {
    /// Creates `MessagesOptions` with the given direction.
    ///
    /// All other parameters will be defaulted.
    pub fn new(dir: Direction) -> Self {
        Self { from: None, to: None, dir, limit: uint!(10), filter: RoomEventFilter::default() }
    }

    /// Creates `MessagesOptions` with `dir` set to `Backward`.
    ///
    /// If no `from` token is set afterwards, pagination will start at the
    /// end of (the accessible part of) the room timeline.
    pub fn backward() -> Self {
        Self::new(Direction::Backward)
    }

    /// Creates `MessagesOptions` with `dir` set to `Forward`.
    ///
    /// If no `from` token is set afterwards, pagination will start at the
    /// beginning of (the accessible part of) the room timeline.
    pub fn forward() -> Self {
        Self::new(Direction::Forward)
    }

    /// Creates a new `MessagesOptions` from `self` with the `from` field set to
    /// the given value.
    ///
    /// Since the field is public, you can also assign to it directly. This
    /// method merely acts as a shorthand for that, because it is very
    /// common to set this field.
    pub fn from<'a>(self, from: impl Into<Option<&'a str>>) -> Self {
        Self { from: from.into().map(ToOwned::to_owned), ..self }
    }

    pub(super) fn into_request(self, room_id: &RoomId) -> get_message_events::v3::Request {
        assign!(get_message_events::v3::Request::new(room_id.to_owned(), self.dir), {
            from: self.from,
            to: self.to,
            limit: self.limit,
            filter: self.filter,
        })
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for MessagesOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { from, to, dir, limit, filter } = self;

        let mut s = f.debug_struct("MessagesOptions");
        s.maybe_field("from", from).maybe_field("to", to).field("dir", dir).field("limit", limit);
        if !filter.is_empty() {
            s.field("filter", filter);
        }
        s.finish()
    }
}

/// The result of a [`super::Room::messages`] call.
///
/// In short, this is a possibly decrypted version of the response of a
/// `room/messages` api call.
#[derive(Debug, Default)]
pub struct Messages {
    /// The token the pagination starts from.
    pub start: String,

    /// The token the pagination ends at.
    pub end: Option<String>,

    /// A list of room events.
    pub chunk: Vec<TimelineEvent>,

    /// A list of state events relevant to showing the `chunk`.
    pub state: Vec<Raw<AnyStateEvent>>,
}

/// The result of a [`super::Room::event_with_context`] query.
///
/// This is a wrapper around
/// [`ruma::api::client::context::get_context::v3::Response`], with events
/// decrypted if needs be.
#[derive(Debug, Default)]
pub struct EventWithContextResponse {
    /// The event targeted by the /context query.
    pub event: Option<TimelineEvent>,

    /// Events before the target event, if a non-zero context size was
    /// requested.
    ///
    /// Like the corresponding Ruma response, these are in reverse chronological
    /// order.
    pub events_before: Vec<TimelineEvent>,

    /// Events after the target event, if a non-zero context size was requested.
    ///
    /// Like the corresponding Ruma response, these are in chronological order.
    pub events_after: Vec<TimelineEvent>,

    /// Token to paginate backwards, aka "start" token.
    pub prev_batch_token: Option<String>,

    /// Token to paginate forwards, aka "end" token.
    pub next_batch_token: Option<String>,

    /// State events related to the request.
    ///
    /// If lazy-loading of members was requested, this may contain room
    /// membership events.
    pub state: Vec<Raw<AnyStateEvent>>,
}

/// Options for [super::Room::list_threads].
#[derive(Debug, Default)]
pub struct ListThreadsOptions {
    /// An extra filter to select which threads should be returned.
    pub include_threads: IncludeThreads,

    /// The token to start returning events from.
    ///
    /// This token can be obtained from a [`ThreadRoots::prev_batch_token`]
    /// returned by a previous call to [`super::Room::list_threads()`].
    ///
    /// If `from` isn't provided the homeserver shall return a list of thread
    /// roots from end of the timeline history.
    pub from: Option<String>,

    /// The maximum number of events to return.
    ///
    /// Default: 10.
    pub limit: Option<UInt>,
}

impl ListThreadsOptions {
    /// Converts the thread options into a Ruma request.
    pub(super) fn into_request(self, room_id: &RoomId) -> get_threads::v1::Request {
        assign!(get_threads::v1::Request::new(room_id.to_owned()), {
            from: self.from,
            include: self.include_threads,
            limit: self.limit,
        })
    }
}

/// The result of a [`super::Room::list_threads`] query.
///
/// This is a wrapper around the Ruma equivalent, with events decrypted if needs
/// be.
#[derive(Debug)]
pub struct ThreadRoots {
    /// The events that are thread roots in the current batch.
    pub chunk: Vec<TimelineEvent>,

    /// Token to paginate backwards in a subsequent query to
    /// [`super::Room::list_threads`].
    pub prev_batch_token: Option<String>,
}

/// What kind of relations should be included in a [`super::Room::relations`]
/// query.
#[derive(Clone, Debug, Default)]
pub enum IncludeRelations {
    /// Include all relations independently of their relation type.
    #[default]
    AllRelations,
    /// Include all relations of a given relation type.
    RelationsOfType(RelationType),
    /// Include all relations of a given relation type and event type.
    RelationsOfTypeAndEventType(RelationType, TimelineEventType),
}

/// Options for [`messages`][super::Room::relations].
#[derive(Clone, Debug, Default)]
pub struct RelationsOptions {
    /// The token to start returning events from.
    ///
    /// This token can be obtained from a [`Relations::prev_batch_token`]
    /// returned by a previous call to [`super::Room::relations()`].
    ///
    /// If `from` isn't provided the homeserver shall return a list of thread
    /// roots from end of the timeline history.
    pub from: Option<String>,

    /// The direction to return events in.
    ///
    /// Defaults to backwards.
    pub dir: Direction,

    /// The maximum number of events to return.
    ///
    /// Default: 10.
    pub limit: Option<UInt>,

    /// Optional restrictions on the relations to include based on their type or
    /// event type.
    ///
    /// Defaults to all relations.
    pub include_relations: IncludeRelations,

    /// Whether to include events which relate indirectly to the given event.
    ///
    /// These are events related to the given event via two or more direct
    /// relationships.
    pub recurse: bool,
}

impl RelationsOptions {
    /// Converts this options object into a request, according to the filled
    /// parameters, and returns a canonicalized response.
    pub(super) async fn send(self, room: &Room, event: OwnedEventId) -> Result<Relations> {
        macro_rules! fill_params {
            ($request:expr) => {
                assign! { $request, {
                    from: self.from,
                    dir: self.dir,
                    limit: self.limit,
                    recurse: self.recurse,
                }}
            };
        }

        // This match to common out the different `Response` types into a single one. It
        // would've been nice that Ruma used the same response type for all the
        // responses, but it is likely doing so to guard against possible future
        // changes.
        let (chunk, prev_batch, next_batch, recursion_depth) = match self.include_relations {
            IncludeRelations::AllRelations => {
                let request = fill_params!(relations::get_relating_events::v1::Request::new(
                    room.room_id().to_owned(),
                    event,
                ));
                let response = room.client.send(request).await?;
                (response.chunk, response.prev_batch, response.next_batch, response.recursion_depth)
            }

            IncludeRelations::RelationsOfType(relation_type) => {
                let request =
                    fill_params!(relations::get_relating_events_with_rel_type::v1::Request::new(
                        room.room_id().to_owned(),
                        event,
                        relation_type,
                    ));
                let response = room.client.send(request).await?;
                (response.chunk, response.prev_batch, response.next_batch, response.recursion_depth)
            }

            IncludeRelations::RelationsOfTypeAndEventType(relation_type, timeline_event_type) => {
                let request = fill_params!(
                    relations::get_relating_events_with_rel_type_and_event_type::v1::Request::new(
                        room.room_id().to_owned(),
                        event,
                        relation_type,
                        timeline_event_type,
                    )
                );
                let response = room.client.send(request).await?;
                (response.chunk, response.prev_batch, response.next_batch, response.recursion_depth)
            }
        };

        let push_ctx = room.push_context().await?;
        let chunk = join_all(chunk.into_iter().map(|ev| {
            // Cast safety: an `AnyMessageLikeEvent` is a subset of an `AnyTimelineEvent`.
            room.try_decrypt_event(ev.cast(), push_ctx.as_ref())
        }))
        .await;

        Ok(Relations {
            chunk,
            prev_batch_token: prev_batch,
            next_batch_token: next_batch,
            recursion_depth,
        })
    }
}

/// The result of a [`super::Room::relations`] query.
///
/// This is a wrapper around the Ruma equivalents, with events decrypted if
/// needs be.
#[derive(Debug)]
pub struct Relations {
    /// The events related to the specified event from the request.
    ///
    /// Note: the events will be sorted according to the `dir` parameter:
    /// - if the direction was backwards, then the events will be ordered in
    ///   reverse topological order.
    /// - if the direction was forwards, then the events will be ordered in
    ///   topological order.
    pub chunk: Vec<TimelineEvent>,

    /// An opaque string representing a pagination token to retrieve the
    /// previous batch of events.
    pub prev_batch_token: Option<String>,

    /// An opaque string representing a pagination token to retrieve the next
    /// batch of events.
    pub next_batch_token: Option<String>,

    /// If [`RelationsOptions::recurse`] was set, the depth to which the server
    /// recursed.
    pub recursion_depth: Option<UInt>,
}
