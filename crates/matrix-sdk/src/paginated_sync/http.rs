// Copyright 2026 The Matrix.org Foundation C.I.C.
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
// See the License for that specific language governing permissions and
// limitations under the License.

//! HTTP types for Paginated Sync (MSC4525), a dialect of Simplified Sliding
//! Sync (MSC4186) without lists, ranges or subscriptions.
//!
//! The per-room results and the extensions are exactly MSC4186's, so those
//! types are reused verbatim from [`v5`]; only the request shape and the
//! top-level response differ.

use std::{collections::BTreeMap, time::Duration};

use ruma::{
    OwnedRoomId, UInt,
    api::{auth_scheme::AccessToken, client::sync::sync_events::v5, request, response},
    events::StateEventType,
    metadata,
    serde::duration::opt_ms,
};

metadata! {
    method: POST,
    rate_limited: false,
    authentication: AccessToken,
    history: {
        unstable("org.matrix.msc4525") => "/_matrix/client/unstable/org.matrix.msc4525/sync",
    }
}

/// Request type for the paginated sync endpoint.
#[request]
#[derive(Default)]
pub struct Request {
    /// A point in time to continue a sync from, from the `pos` field of a
    /// previous response. `None` starts a new connection.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ruma_api(query)]
    pub pos: Option<String>,

    /// A unique string identifier for this connection to the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conn_id: Option<String>,

    /// The maximum time to poll before responding to this request. Ignored by
    /// the server whenever it has rooms pending for the client.
    #[serde(with = "opt_ms", default, skip_serializing_if = "Option::is_none")]
    #[ruma_api(query)]
    pub timeout: Option<Duration>,

    /// The maximum number of rooms to return in the response.
    pub page_size: UInt,

    /// The maximum number of new timeline events to return per room, beyond
    /// which the room is returned `limited` with a `prev_batch` (an explicit
    /// per-room gap).
    pub limit: UInt,

    /// The number of most-recent timeline events to return for a room which
    /// has not previously been sent on this connection. Defaults to `limit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<UInt>,

    /// The state to return for every room, with MSC4186 `required_state`
    /// semantics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_state: Vec<(StateEventType, String)>,

    /// Extensions, exactly as MSC4186's.
    #[serde(default, skip_serializing_if = "v5::request::Extensions::is_empty")]
    pub extensions: v5::request::Extensions,
}

impl Request {
    /// Creates an empty `Request`.
    pub fn new() -> Self {
        Default::default()
    }
}

/// Response type for the paginated sync endpoint.
#[response]
pub struct Response {
    /// The token to supply in the `pos` parameter of the next request.
    pub pos: String,

    /// The updated rooms, exactly as MSC4186's.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rooms: BTreeMap<OwnedRoomId, v5::response::Room>,

    /// Extensions, exactly as MSC4186's.
    #[serde(default, skip_serializing_if = "v5::response::Extensions::is_empty")]
    pub extensions: v5::response::Extensions,

    /// The number of further rooms with undelivered updates which did not fit
    /// into `page_size`. While non-zero, the client should sync again
    /// immediately to drain the backlog.
    #[serde(default, skip_serializing_if = "ruma::serde::is_default")]
    pub pending: UInt,

    /// The total number of rooms in the user's account, for cold-start
    /// progress reporting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_rooms: Option<UInt>,
}

impl Response {
    /// Creates a new `Response` with the given `pos`.
    pub fn new(pos: String) -> Self {
        Self {
            pos,
            rooms: Default::default(),
            extensions: Default::default(),
            pending: UInt::default(),
            total_rooms: None,
        }
    }
}

impl From<Response> for v5::Response {
    /// Convert into an MSC4186 response so that the entire response-processing
    /// pipeline (rooms, timeline, state, extensions) can be reused unchanged.
    fn from(response: Response) -> Self {
        let mut v5_response = v5::Response::new(response.pos);
        v5_response.rooms = response.rooms;
        v5_response.extensions = response.extensions;
        v5_response
    }
}
