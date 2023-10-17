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

//! No I/O logic of the [`WidgetDriver`].

#![warn(unreachable_pub)]

use serde::Serialize;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{error, instrument};
use uuid::Uuid;

use self::to_widget::{RequestPermissions, ToWidgetRequest};

mod actions;
mod events;
mod openid;
mod outgoing;
#[cfg(test)]
mod tests;
mod to_widget;

pub(crate) use self::{
    actions::{Action, SendEventCommand},
    events::Event,
};
#[cfg(doc)]
use super::WidgetDriver;

/// No I/O state machine.
///
/// Handles interactions with the widget as well as the `MatrixDriver`.
pub(crate) struct WidgetMachine {
    widget_id: String,
    actions_sender: UnboundedSender<Action>,
}

impl WidgetMachine {
    /// Creates a new instance of a client widget API state machine.
    /// Returns the client api handler as well as the channel to receive
    /// actions (commands) from the client.
    pub(crate) fn new(
        widget_id: String,
        init_on_content_load: bool,
    ) -> (Self, UnboundedReceiver<Action>) {
        let (actions_sender, actions_receiver) = unbounded_channel();
        let this = Self { widget_id, actions_sender };

        if !init_on_content_load {
            this.send_to_widget(RequestPermissions {});
        }

        (this, actions_receiver)
    }

    #[instrument(skip_all, fields(action = T::ACTION))]
    fn send_to_widget<T: ToWidgetRequest>(&self, to_widget_request: T) {
        #[derive(Serialize)]
        #[serde(tag = "api", rename = "toWidget", rename_all = "camelCase")]
        struct ToWidgetRequestSerHelper<'a, T> {
            widget_id: &'a str,
            request_id: Uuid,
            action: &'static str,
            data: T,
        }

        let full_request = ToWidgetRequestSerHelper {
            widget_id: &self.widget_id,
            request_id: Uuid::new_v4(),
            action: T::ACTION,
            data: to_widget_request,
        };
        let serialized = match serde_json::to_string(&full_request) {
            Ok(msg) => msg,
            Err(e) => {
                error!("Failed to serialize outgoing message: {e}");
                return;
            }
        };

        if let Err(e) = self.actions_sender.send(Action::SendToWidget(serialized)) {
            error!("Failed to send action: {e}");
        }
    }

    /// Processes an incoming event (an incoming raw message from a widget,
    /// or a data produced as a result of a previously sent `Action`).
    /// Produceses a list of actions that the client must perform.
    pub(crate) fn process(&mut self, _event: Event) {
        // TODO: Process the event.
    }
}
