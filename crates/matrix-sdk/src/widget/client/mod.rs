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

#![warn(unreachable_pub)]

use ruma::OwnedRoomId;
use serde_json::from_str as from_json;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

pub(crate) use self::{
    actions::{Action, SendEventCommand},
    events::Event,
};
use self::{
    incoming::{Context, HandleResult, IncomingMessage},
    outgoing::RequestSender,
};

mod actions;
mod events;
mod incoming;
mod openid;
mod outgoing;

/// Settings for the client widget API state machine.
#[derive(Clone, Debug)]
pub(crate) struct ClientApiSettings {
    #[allow(dead_code)]
    pub(crate) init_on_content_load: bool,
    #[allow(dead_code)]
    pub(crate) room_id: OwnedRoomId,
}

/// State machine that handles the client widget API interractions.
pub(crate) struct ClientApi {
    settings: ClientApiSettings,
    tx: UnboundedSender<Action>,
}

impl ClientApi {
    /// Creates a new instance of a client widget API state machine.
    /// Returns the client api handler as well as the channel to receive
    /// actions (commands) from the client.
    pub(crate) fn new(settings: ClientApiSettings) -> (Self, UnboundedReceiver<Action>) {
        let (tx, rx) = unbounded_channel();
        (Self { settings, tx }, rx)
    }

    /// Processes an incoming event (an incoming raw message from a widget,
    /// or a data produced as a result of a previously sent `Action`).
    /// Produceses a list of actions that the client must perform.
    pub(crate) fn process(&mut self, event: Event) {
        match event {
            Event::MessageFromWidget(raw) => {
                match from_json::<IncomingMessage>(&raw) {
                    Ok(IncomingMessage::FromWidget(req)) => {
                        let ctx = Context {
                            permissions: None,
                            settings: self.settings.clone(),
                            matrix: Box::new(RequestSender {}),
                        };

                        let sink = self.tx.clone();
                        tokio::spawn(async move {
                            let HandleResult { response, action } = req.handle(ctx).await;
                            let _ = sink.send(Action::SendToWidget(response));
                            if let Some(_post_action) = action {
                                // TODO: Implement this.
                            }
                        });
                    }
                    Err(_err) => {
                        // TODO: Handle error.
                    }
                }
            }
            _ => {
                // TODO: Implement this.
            }
        }
    }
}
