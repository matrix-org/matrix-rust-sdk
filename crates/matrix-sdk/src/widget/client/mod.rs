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

//! Internal client widget API implementation.

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

pub use self::{
    actions::{Action, SendEventCommand},
    events::Event,
};

mod actions;
mod events;

/// State machine that handles the client widget API interractions.
pub struct ClientApi;

impl ClientApi {
    /// Creates a new instance of a client widget API state machine.
    /// Returns the client api handler as well as the channel to receive
    /// actions (commands) from the client.
    pub fn new() -> (Self, UnboundedReceiver<Action>) {
        let (_tx, rx) = unbounded_channel();
        (Self, rx)
    }

    /// Processes an incoming event (an incoming raw message from a widget,
    /// or a data produced as a result of a previously sent `Action`).
    /// Produceses a list of actions that the client must perform.
    pub fn process(&mut self, _event: Event) {
        // TODO: Process the event.
    }
}
