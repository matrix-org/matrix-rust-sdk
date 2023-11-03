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

use matrix_sdk_common::{debug::DebugStructExt as _, deserialized_responses::TimelineEvent};
use ruma::{
    api::{
        client::{filter::RoomEventFilter, message::get_message_events},
        Direction,
    },
    assign,
    events::AnyStateEvent,
    serde::Raw,
    uint, RoomId, UInt,
};

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

/// The result of a `Room::messages` call.
///
/// In short, this is a possibly decrypted version of the response of a
/// `room/messages` api call.
#[derive(Debug)]
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
