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

//! Widget API implementation.

use std::{fmt, time::Duration};

use async_channel::{Receiver, Sender};
use ruma::api::client::delayed_events::DelayParameters;
use serde::de::{self, Deserialize, Deserializer, Visitor};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio_util::sync::{CancellationToken, DropGuard};

use self::{
    machine::{
        Action, IncomingMessage, MatrixDriverRequestData, MatrixDriverResponse, SendEventRequest,
        WidgetMachine,
    },
    matrix::MatrixDriver,
};
use crate::{room::Room, Result};

mod capabilities;
mod filter;
mod machine;
mod matrix;
mod settings;

pub use self::{
    capabilities::{Capabilities, CapabilitiesProvider},
    filter::{EventFilter, MessageLikeEventFilter, StateEventFilter},
    settings::{
        ClientProperties, EncryptionSystem, VirtualElementCallWidgetOptions, WidgetSettings,
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
        let (incoming_msg_tx, mut incoming_msg_rx) = unbounded_channel();

        // Forward all of the incoming messages from the widget.
        tokio::spawn({
            let incoming_msg_tx = incoming_msg_tx.clone();
            let from_widget_rx = self.from_widget_rx.clone();
            async move {
                while let Ok(msg) = from_widget_rx.recv().await {
                    let _ = incoming_msg_tx.send(IncomingMessage::WidgetMessage(msg));
                }
            }
        });

        // Create widget API machine.
        let (mut widget_machine, initial_actions) = WidgetMachine::new(
            self.settings.widget_id().to_owned(),
            room.room_id().to_owned(),
            self.settings.init_on_content_load(),
        );

        let matrix_driver = MatrixDriver::new(room.clone());

        // Process initial actions that "initialise" the widget api machine.
        for action in initial_actions {
            self.process_action(&matrix_driver, &incoming_msg_tx, &capabilities_provider, action)
                .await?;
        }

        // Process incoming messages as they're coming in.
        while let Some(msg) = incoming_msg_rx.recv().await {
            for action in widget_machine.process(msg) {
                self.process_action(
                    &matrix_driver,
                    &incoming_msg_tx,
                    &capabilities_provider,
                    action,
                )
                .await?;
            }
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

                    MatrixDriverRequestData::ReadMessageLikeEvent(cmd) => matrix_driver
                        .read_message_like_events(cmd.event_type.clone(), cmd.limit)
                        .await
                        .map(MatrixDriverResponse::MatrixEventRead),

                    MatrixDriverRequestData::ReadStateEvent(cmd) => matrix_driver
                        .read_state_events(cmd.event_type.clone(), &cmd.state_key)
                        .await
                        .map(MatrixDriverResponse::MatrixEventRead),

                    MatrixDriverRequestData::SendMatrixEvent(req) => {
                        let SendEventRequest { event_type, state_key, content, delay } = req;
                        // The widget api action does not use the unstable prefix:
                        // `org.matrix.msc4140.delay` so we
                        // cannot use the `DelayParameters` here and need to convert
                        // manually.
                        let delay_event_parameter = delay.map(|d| DelayParameters::Timeout {
                            timeout: Duration::from_millis(d),
                        });
                        matrix_driver
                            .send(event_type, state_key, content, delay_event_parameter)
                            .await
                            .map(MatrixDriverResponse::MatrixEventSent)
                    }

                    MatrixDriverRequestData::UpdateDelayedEvent(req) => matrix_driver
                        .update_delayed_event(req.delay_id, req.action)
                        .await
                        .map(MatrixDriverResponse::MatrixDelayedEventUpdate),
                };

                // Forward the matrix driver response to the incoming message stream.
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

                let mut matrix = matrix_driver.events();
                let incoming_msg_tx = incoming_msg_tx.clone();

                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = stop_forwarding.cancelled() => {
                                // Upon cancellation, stop this task.
                                return;
                            }

                            Some(event) = matrix.recv() => {
                                // Forward all events to the incoming messages stream.
                                let _ = incoming_msg_tx.send(IncomingMessage::MatrixEventReceived(event));
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
