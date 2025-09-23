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

#![allow(rustdoc::private_intra_doc_links)]
#![doc = include_str!("README.md")]

use std::{fmt, time::Duration};

use async_channel::{Receiver, Sender};
use futures_util::StreamExt;
use matrix_sdk_common::executor::spawn;
use ruma::api::client::delayed_events::DelayParameters;
use serde::de::{self, Deserialize, Deserializer, Visitor};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::{CancellationToken, DropGuard};

use self::{
    machine::{
        Action, IncomingMessage, MatrixDriverRequestData, MatrixDriverResponse, SendEventRequest,
        WidgetMachine,
    },
    matrix::MatrixDriver,
};
use crate::{Result, room::Room};

mod capabilities;
mod filter;
mod machine;
mod matrix;
mod settings;

pub use self::{
    capabilities::{Capabilities, CapabilitiesProvider},
    filter::{Filter, MessageLikeEventFilter, StateEventFilter, ToDeviceEventFilter},
    settings::{
        ClientProperties, EncryptionSystem, Intent, VirtualElementCallWidgetConfig,
        VirtualElementCallWidgetProperties, WidgetSettings,
    },
};

/// An object that handles all interactions of a widget living inside a webview
/// or iframe with the Matrix world.
#[derive(Debug)]
pub struct WidgetDriver {
    settings: WidgetSettings,

    /// Raw incoming messages from the widget (normally formatted as JSON).
    ///
    /// These can be both requests and responses.
    from_widget_rx: Receiver<String>,

    /// Raw outgoing messages from the client (SDK) to the widget (normally
    /// formatted as JSON).
    ///
    /// These can be both requests and responses.
    to_widget_tx: Sender<String>,

    /// Drop guard for an event handler forwarding all events from the Matrix
    /// room to the widget.
    ///
    /// Only set if a subscription happened ([`Action::Subscribe`]).
    event_forwarding_guard: Option<DropGuard>,
}

/// A handle that encapsulates the communication between a widget driver and the
/// corresponding widget (inside a webview or iframe).
#[derive(Clone, Debug)]
pub struct WidgetDriverHandle {
    /// Raw incoming messages from the widget driver to the widget (normally
    /// formatted as JSON).
    ///
    /// These can be both requests and responses. Users of this API should not
    /// care what's what though because they are only supposed to forward
    /// messages between the webview / iframe, and the SDK's widget driver.
    to_widget_rx: Receiver<String>,

    /// Raw outgoing messages from the widget to the widget driver (normally
    /// formatted as JSON).
    ///
    /// These can be both requests and responses. Users of this API should not
    /// care what's what though because they are only supposed to forward
    /// messages between the webview / iframe, and the SDK's widget driver.
    from_widget_tx: Sender<String>,
}

impl WidgetDriverHandle {
    /// Receive a message from the widget driver.
    ///
    /// The message must be passed on to the widget.
    ///
    /// Returns `None` if the widget driver is no longer running.
    pub async fn recv(&self) -> Option<String> {
        self.to_widget_rx.recv().await.ok()
    }

    /// Send a message from the widget to the widget driver.
    ///
    /// Returns `false` if the widget driver is no longer running.
    pub async fn send(&self, message: String) -> bool {
        self.from_widget_tx.send(message).await.is_ok()
    }
}

impl WidgetDriver {
    /// Creates a new `WidgetDriver` and a corresponding set of channels to let
    /// the widget (inside a webview or iframe) communicate with it.
    pub fn new(settings: WidgetSettings) -> (Self, WidgetDriverHandle) {
        let (from_widget_tx, from_widget_rx) = async_channel::unbounded();
        let (to_widget_tx, to_widget_rx) = async_channel::unbounded();

        let driver = Self { settings, from_widget_rx, to_widget_tx, event_forwarding_guard: None };
        let channels = WidgetDriverHandle { from_widget_tx, to_widget_rx };

        (driver, channels)
    }

    /// Run client widget API state machine in a given joined `room` forever.
    ///
    /// The function returns once the widget is disconnected or any terminal
    /// error occurs.
    pub async fn run(
        mut self,
        room: Room,
        capabilities_provider: impl CapabilitiesProvider,
    ) -> Result<(), ()> {
        // Create a channel so that we can conveniently send all messages to it.
        //
        // It will receive:
        // - all incoming messages from the widget
        // - all responses from the Matrix driver
        // - all events from the Matrix driver, if subscribed
        let (incoming_msg_tx, incoming_msg_rx) = unbounded_channel();

        // Forward all of the incoming messages from the widget.
        // TODO: This spawns a detached task, it would be nice to have an owner for this
        // task. One way to achieve this if `WidgetDriver::run()` returns a handle that
        // we can drop which will clean up the task and the channels. It's not too bad,
        // since canelling `run()` will drop the sender this task listens which finishes
        // the task.
        spawn({
            let incoming_msg_tx = incoming_msg_tx.clone();
            let from_widget_rx = self.from_widget_rx.clone();

            async move {
                while let Ok(msg) = from_widget_rx.recv().await {
                    let _ = incoming_msg_tx.send(IncomingMessage::WidgetMessage(msg));
                }
            }
        });

        // Create the widget API machine. The widget machine will process messages it
        // receives from the widget and convert it into actions the `MatrixDriver` will
        // then execute on.
        let (mut widget_machine, initial_actions) = WidgetMachine::new(
            self.settings.widget_id().to_owned(),
            room.room_id().to_owned(),
            self.settings.init_on_content_load(),
        );

        let matrix_driver = MatrixDriver::new(room.clone());

        // Convert the incoming message receiver into a stream of actions.
        let stream = UnboundedReceiverStream::new(incoming_msg_rx)
            .flat_map(|message| tokio_stream::iter(widget_machine.process(message)));

        // Let's combine our set of initial actions with the stream of received actions.
        let mut combined = tokio_stream::iter(initial_actions).chain(stream);

        // Let's now process all actions we receive forever.
        while let Some(action) = combined.next().await {
            self.process_action(&matrix_driver, &incoming_msg_tx, &capabilities_provider, action)
                .await?;
        }

        Ok(())
    }

    /// Process a single [`Action`].
    async fn process_action(
        &mut self,
        matrix_driver: &MatrixDriver,
        incoming_msg_tx: &UnboundedSender<IncomingMessage>,
        capabilities_provider: &impl CapabilitiesProvider,
        action: Action,
    ) -> Result<(), ()> {
        match action {
            Action::SendToWidget(msg) => {
                self.to_widget_tx.send(msg).await.map_err(|_| ())?;
            }

            Action::MatrixDriverRequest { request_id, data } => {
                let response = match data {
                    MatrixDriverRequestData::AcquireCapabilities(cmd) => {
                        let obtained = capabilities_provider
                            .acquire_capabilities(cmd.desired_capabilities)
                            .await;
                        Ok(MatrixDriverResponse::CapabilitiesAcquired(obtained))
                    }

                    MatrixDriverRequestData::GetOpenId => {
                        matrix_driver.get_open_id().await.map(MatrixDriverResponse::OpenIdReceived)
                    }

                    MatrixDriverRequestData::ReadEvents(cmd) => matrix_driver
                        .read_events(cmd.event_type.into(), cmd.state_key, cmd.limit)
                        .await
                        .map(MatrixDriverResponse::EventsRead),

                    MatrixDriverRequestData::ReadState(cmd) => matrix_driver
                        .read_state(cmd.event_type.into(), &cmd.state_key)
                        .await
                        .map(MatrixDriverResponse::StateRead),

                    MatrixDriverRequestData::SendEvent(req) => {
                        let SendEventRequest { event_type, state_key, content, delay } = req;
                        // The widget api action does not use the unstable prefix:
                        // `org.matrix.msc4140.delay` so we
                        // cannot use the `DelayParameters` here and need to convert
                        // manually.
                        let delay_event_parameter = delay.map(|d| DelayParameters::Timeout {
                            timeout: Duration::from_millis(d),
                        });
                        matrix_driver
                            .send(event_type.into(), state_key, content, delay_event_parameter)
                            .await
                            .map(MatrixDriverResponse::EventSent)
                    }

                    MatrixDriverRequestData::UpdateDelayedEvent(req) => matrix_driver
                        .update_delayed_event(req.delay_id, req.action)
                        .await
                        .map(MatrixDriverResponse::DelayedEventUpdated),

                    MatrixDriverRequestData::SendToDeviceEvent(send_to_device_request) => {
                        matrix_driver
                            .send_to_device(
                                send_to_device_request.event_type.into(),
                                send_to_device_request.messages,
                            )
                            .await
                            .map(MatrixDriverResponse::ToDeviceSent)
                    }
                };

                // Forward the Matrix driver response to the incoming message stream.
                incoming_msg_tx
                    .send(IncomingMessage::MatrixDriverResponse { request_id, response })
                    .map_err(|_| ())?;
            }

            Action::Subscribe => {
                // Only subscribe if we are not already subscribed.
                if self.event_forwarding_guard.is_some() {
                    return Ok(());
                }

                let (stop_forwarding, guard) = {
                    let token = CancellationToken::new();
                    (token.child_token(), token.drop_guard())
                };

                self.event_forwarding_guard = Some(guard);

                let mut events = matrix_driver.events();
                let mut state_updates = matrix_driver.state_updates();
                let mut to_device_events = matrix_driver.to_device_events();
                let incoming_msg_tx = incoming_msg_tx.clone();

                spawn(async move {
                    loop {
                        tokio::select! {
                            _ = stop_forwarding.cancelled() => {
                                // Upon cancellation, stop this task.
                                return;
                            }

                            Some(event) = events.recv() => {
                                // Forward all events to the incoming messages stream.
                                let _ = incoming_msg_tx.send(IncomingMessage::MatrixEventReceived(event));
                            }

                            Ok(state) = state_updates.recv() => {
                                // Forward all state updates to the incoming messages stream.
                                let _ = incoming_msg_tx.send(IncomingMessage::StateUpdateReceived(state));
                            }

                            Some(event) = to_device_events.recv() => {
                                // Forward all events to the incoming messages stream.
                                let _ = incoming_msg_tx.send(IncomingMessage::ToDeviceReceived(event));
                            }
                        }
                    }
                });
            }

            Action::Unsubscribe => {
                self.event_forwarding_guard = None;
            }
        }

        Ok(())
    }
}

// TODO: Decide which module this type should live in
#[derive(Clone, Debug)]
pub(crate) enum StateKeySelector {
    Key(String),
    Any,
}

impl<'de> Deserialize<'de> for StateKeySelector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StateKeySelectorVisitor;

        impl Visitor<'_> for StateKeySelectorVisitor {
            type Value = StateKeySelector;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a string or `true`")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if v {
                    Ok(StateKeySelector::Any)
                } else {
                    Err(E::invalid_value(de::Unexpected::Bool(v), &self))
                }
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_string(v.to_owned())
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StateKeySelector::Key(v))
            }
        }

        deserializer.deserialize_any(StateKeySelectorVisitor)
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use serde_json::json;

    use super::StateKeySelector;

    #[test]
    fn state_key_selector_from_true() {
        let state_key = serde_json::from_value(json!(true)).unwrap();
        assert_matches!(state_key, StateKeySelector::Any);
    }

    #[test]
    fn state_key_selector_from_string() {
        let state_key = serde_json::from_value(json!("test")).unwrap();
        assert_matches!(state_key, StateKeySelector::Key(k) if k == "test");
    }

    #[test]
    fn state_key_selector_from_false() {
        let result = serde_json::from_value::<StateKeySelector>(json!(false));
        assert_matches!(result, Err(e) if e.is_data());
    }

    #[test]
    fn state_key_selector_from_number() {
        let result = serde_json::from_value::<StateKeySelector>(json!(5));
        assert_matches!(result, Err(e) if e.is_data());
    }
}
