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

use std::{borrow::Cow, error::Error, ops::Deref};

use ruma::events::TimelineEventType;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::widget::Permissions;

/// Action (a command) that client (driver) must perform.
#[allow(dead_code)] // TODO: Remove once all actions are implemented.
pub enum Action {
    /// Send a raw message to the widget.
    SendToWidget(String),
    /// Acquire permissions from the user given the set of desired permissions.
    /// Must eventually be answered with `Event::PermissionsAcquired`.
    AcquirePermissions(Command<Permissions>),
    /// Get OpenId token for a given request ID.
    GetOpenId(Command<()>),
    /// Read matrix event(s) that corresponds to the given description.
    ReadMatrixEvent(Command<ReadEventCommand>),
    // Send matrix event that corresponds to the given description.
    SendMatrixEvent(Command<SendEventCommand>),
    /// Subscribe to the events in the *current* room, i.e. a room which this
    /// widget is instantiated with. The client is aware of the room.
    Subscribe,
    /// Unsuscribe from the events in the *current* room. Symmetrical to
    /// `Subscribe`.
    Unsubscribe,
}

/// Command to read matrix event(s).
pub struct ReadEventCommand {
    /// Read event(s) of a given type.
    pub event_type: TimelineEventType,
    /// Limits for the Matrix request.
    pub limit: u32,
}

/// Command to send matrix event.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SendEventCommand {
    #[serde(rename = "type")]
    /// type of an event.
    pub event_type: TimelineEventType,
    /// State key of an event (if it's a state event).
    pub state_key: Option<String>,
    /// Raw content of an event.
    pub content: JsonValue,
}

/// Command that is sent from the client widget API state machine to the
/// client (driver) that must be performed. Once the command is executed,
/// the client will typically generate an `Event` with the result of it.
pub struct Command<T> {
    /// Certain commands are typically answered with certain event once the
    /// command is performed. The api state machine will "tag" each command
    /// with some "cookie" (in this case just an ID), so that once the
    /// result of the execution of this command is received, it could be
    /// matched.
    id: String,
    // Data associated with this command.
    data: T,
}

impl<T> Command<T> {
    /// Consumes the command and produces a command result with given data.
    pub fn result<U, E: Error>(self, result: Result<U, E>) -> CommandResult<U> {
        CommandResult { id: self.id, result: result.map_err(|e| e.to_string().into()) }
    }

    pub fn ok<U>(self, value: U) -> CommandResult<U> {
        CommandResult { id: self.id, result: Ok(value) }
    }
}

impl<T> Deref for Command<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

/// The result of the execution of a command. Note that this type can only be
/// constructed within this module, i.e. it can only be constructed as a result
/// of a command that has been sent from this module, which means that the
/// client (driver) won't be able to send "invalid" commands, because they could
/// only be generated from a `Command` instance.
#[allow(dead_code)] // TODO: Remove once results are used.
pub struct CommandResult<T> {
    /// ID of the command that was executed. See `Command::id` for more details.
    id: String,
    /// Result of the execution of the command.
    result: Result<T, Cow<'static, str>>,
}
