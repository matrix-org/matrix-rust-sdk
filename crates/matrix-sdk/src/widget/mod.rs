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

use async_channel::{Receiver, Sender};
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::{CancellationToken, DropGuard};

use self::{
    machine::{
        Action, IncomingMessage, MatrixDriverRequestData, MatrixDriverResponse, SendEventCommand,
        WidgetMachine,
    },
    matrix::MatrixDriver,
};
use crate::{room::Room, Result};

mod filter;
mod machine;
mod matrix;
mod permissions;
mod settings;

pub use self::{
    filter::{EventFilter, MessageLikeEventFilter, StateEventFilter},
    permissions::{Permissions, PermissionsProvider},
    settings::{ClientProperties, VirtualElementCallWidgetOptions, WidgetSettings},
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

        let driver = Self { settings, from_widget_rx, to_widget_tx };
        let channels = WidgetDriverHandle { from_widget_tx, to_widget_rx };

        (driver, channels)
    }

    /// Starts a client widget API state machine for a given `widget` in a given
    /// joined `room`. The function returns once the widget is disconnected or
    /// any terminal error occurs.
    ///
    /// Not implemented yet! Currently, it does not contain any useful
    /// functionality, it only blindly forwards the messages and returns errors
    /// once a non-implemented part is triggered.
    pub async fn run(
        self,
        room: Room,
        permissions_provider: impl PermissionsProvider,
    ) -> Result<(), ()> {
        let (mut client_api, mut actions) = WidgetMachine::new(
            self.settings.widget_id().to_owned(),
            self.settings.init_on_content_load(),
        );

        // Create a channel so that we can conveniently send all events to it.
        let (events_tx, mut events_rx) = unbounded_channel();

        // Forward all of the incoming messages from the widget to the `events_tx`.
        let tx = events_tx.clone();
        tokio::spawn(async move {
            while let Ok(msg) = self.from_widget_rx.recv().await {
                let _ = tx.send(IncomingMessage::WidgetMessage(msg));
            }
        });

        // Forward all of the incoming events to the `ClientApi` implementation.
        tokio::spawn(async move {
            while let Some(event) = events_rx.recv().await {
                client_api.process(event);
            }
        });

        // Process events that we receive **from** the client api implementation,
        // i.e. the commands (actions) that the client sends to us.
        let matrix_driver = MatrixDriver::new(room);
        let mut event_forwarding_guard: Option<DropGuard> = None;
        while let Some(action) = actions.recv().await {
            match action {
                Action::SendToWidget(msg) => self.to_widget_tx.send(msg).await.map_err(|_| ())?,
                Action::MatrixDriverRequest { request_id: id, data } => {
                    let event = match data {
                        MatrixDriverRequestData::AcquirePermissions(cmd) => {
                            let obtained = permissions_provider
                                .acquire_permissions(cmd.desired_permissions.clone())
                                .await;

                            IncomingMessage::PermissionsAcquired(MatrixDriverResponse::ok(
                                id, obtained,
                            ))
                        }
                        MatrixDriverRequestData::GetOpenId => {
                            let result =
                                MatrixDriverResponse::new(id, matrix_driver.get_open_id().await);
                            IncomingMessage::OpenIdReceived(result)
                        }
                        MatrixDriverRequestData::ReadMessageLikeEvent(cmd) => {
                            let events = matrix_driver
                                .read_message_like_events(cmd.event_type.clone(), cmd.limit)
                                .await;
                            IncomingMessage::MatrixEventRead(MatrixDriverResponse::new(id, events))
                        }
                        MatrixDriverRequestData::ReadStateEvent(cmd) => {
                            let events = matrix_driver
                                .read_state_events(cmd.event_type.clone(), &cmd.state_key)
                                .await;
                            IncomingMessage::MatrixEventRead(MatrixDriverResponse::new(id, events))
                        }
                        MatrixDriverRequestData::SendMatrixEvent(cmd) => {
                            let SendEventCommand { event_type, state_key, content } = cmd.clone();
                            let matrix_event_id =
                                matrix_driver.send(event_type, state_key, content).await;
                            IncomingMessage::MatrixEventSent(MatrixDriverResponse::new(
                                id,
                                matrix_event_id,
                            ))
                        }
                    };

                    events_tx.send(event).map_err(|_| ())?;
                }
                Action::Subscribe => {
                    // Only subscribe if we are not already subscribed.
                    if event_forwarding_guard.is_none() {
                        let (stop_forwarding, guard) = {
                            let token = CancellationToken::new();
                            (token.child_token(), token.drop_guard())
                        };

                        event_forwarding_guard = Some(guard);
                        let (mut matrix, events_tx) = (matrix_driver.events(), events_tx.clone());
                        tokio::spawn(async move {
                            loop {
                                tokio::select! {
                                    _ = stop_forwarding.cancelled() => { return }
                                    Some(event) = matrix.recv() => {
                                        let _ = events_tx.send(IncomingMessage::MatrixEventReceived(event));
                                    }
                                }
                            }
                        });
                    }
                }
                Action::Unsubscribe => {
                    event_forwarding_guard = None;
                }
            }
        }

        Ok(())
    }
}

// TODO: Decide which module this type should live in
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) enum StateKeySelector {
    Key(String),
    Any,
}
