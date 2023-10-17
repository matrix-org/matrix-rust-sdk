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

use std::error::Error;

use ruma::events::{MessageLikeEventType, StateEventType, TimelineEventType};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use uuid::Uuid;

use super::driver_req::AcquirePermissions;
use crate::widget::StateKeySelector;

/// Action (a command) that client (driver) must perform.
#[derive(Debug)]
pub(crate) enum Action {
    /// Send a raw message to the widget.
    SendToWidget(String),

    /// Command that is sent from the client widget API state machine to the
    /// client (driver) that must be performed. Once the command is executed,
    /// the client will typically generate an `Event` with the result of it.
    MatrixDriverRequest {
        /// Certain commands are typically answered with certain event once the
        /// command is performed. The api state machine will "tag" each command
        /// with some "cookie" (in this case just an ID), so that once the
        /// result of the execution of this command is received, it could be
        /// matched.
        request_id: Uuid,

        /// Data associated with this command.
        data: MatrixDriverRequestData,
    },

    /// Subscribe to the events in the *current* room, i.e. a room which this
    /// widget is instantiated with. The client is aware of the room.
    #[allow(dead_code)]
    Subscribe,

    /// Unsuscribe from the events in the *current* room. Symmetrical to
    /// `Subscribe`.
    #[allow(dead_code)]
    Unsubscribe,
}

/// Command to read matrix message event(s).
#[derive(Debug)]
pub(crate) struct ReadMessageLikeEventCommand {
    /// The event type to read.
    pub(crate) event_type: MessageLikeEventType,

    /// The maximum number of events to return.
    pub(crate) limit: u32,
}

/// Command to read matrix state event(s).
#[derive(Debug)]
pub(crate) struct ReadStateEventCommand {
    /// The event type to read.
    pub(crate) event_type: StateEventType,

    /// The `state_key` to read, or `Any` to receive any/all events of the given
    /// type, regardless of their `state_key`.
    pub(crate) state_key: StateKeySelector,
}

/// Command to send matrix event.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct SendEventCommand {
    #[serde(rename = "type")]
    /// type of an event.
    pub(crate) event_type: TimelineEventType,
    /// State key of an event (if it's a state event).
    pub(crate) state_key: Option<String>,
    /// Raw content of an event.
    pub(crate) content: JsonValue,
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum MatrixDriverRequestData {
    /// Acquire permissions from the user given the set of desired permissions.
    /// Must eventually be answered with `Event::PermissionsAcquired`.
    AcquirePermissions(AcquirePermissions),

    /// Get OpenId token for a given request ID.
    GetOpenId,

    /// Read message event(s).
    ReadMessageLikeEvent(ReadMessageLikeEventCommand),

    /// Read state event(s).
    ReadStateEvent(ReadStateEventCommand),

    /// Send matrix event that corresponds to the given description.
    SendMatrixEvent(SendEventCommand),
}

/// The result of the execution of a command. Note that this type can only be
/// constructed within this module, i.e. it can only be constructed as a result
/// of a command that has been sent from this module, which means that the
/// client (driver) won't be able to send "invalid" commands, because they could
/// only be generated from a `Command` instance.
#[allow(dead_code)] // TODO: Remove once results are used.
pub(crate) struct MatrixDriverResponse<T> {
    /// ID of the command that was executed. See `Command::id` for more details.
    pub(super) request_id: Uuid,
    /// Result of the execution of the command.
    pub(super) result: Result<T, String>,
}

impl<T> MatrixDriverResponse<T> {
    pub(crate) fn new<E: Error>(request_id: Uuid, result: Result<T, E>) -> Self {
        Self { request_id, result: result.map_err(|e| e.to_string()) }
    }

    pub(crate) fn ok(request_id: Uuid, value: T) -> Self {
        Self { request_id, result: Ok(value) }
    }
}
