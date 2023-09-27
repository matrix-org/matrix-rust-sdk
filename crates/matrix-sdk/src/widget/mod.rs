//! Widget API implementation.

use async_channel::{Receiver, Sender};
use tokio::sync::mpsc::unbounded_channel;

use crate::room::Room;

mod client;
mod filter;
mod permissions;

use self::client::{Action, ClientApi, Event};
pub use self::{
    filter::{EventFilter, MessageLikeEventFilter, StateEventFilter},
    permissions::{Permissions, PermissionsProvider},
};

/// An object that handles all interactions of a widget living inside a webview
/// or iframe with the Matrix world.
#[derive(Debug)]
pub struct WidgetDriver {
    #[allow(dead_code)]
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

/// Information about a widget.
#[derive(Debug)]
pub struct WidgetSettings {
    /// Widget's unique identifier.
    pub id: String,
    /// Whether or not the widget should be initialized on load message
    /// (`ContentLoad` message), or upon creation/attaching of the widget to
    /// the SDK's state machine that drives the API.
    pub init_on_load: bool,
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
        _room: Room,
        _permissions_provider: impl PermissionsProvider,
    ) -> Result<(), ()> {
        // Create a channel so that we can conveniently send all events to it.
        let (events_tx, mut events_rx) = unbounded_channel();

        // Forward all incoming raw messages into events and send them to the sink.
        // Equivalent of the:
        // `from.map(|m| Ok(Event::MessageFromWidget(msg)).forward(events_tx)`,
        // but apparently `UnboundedSender<T>` does not implement `Sink<T>`.
        let tx = events_tx.clone();
        tokio::spawn(async move {
            while let Ok(msg) = self.from_widget_rx.recv().await {
                let _ = tx.send(Event::MessageFromWidget(msg));
            }
        });

        // Process events by passing them to the `ClientApi` implementation.
        let mut client_api = ClientApi::new();
        while let Some(event) = events_rx.recv().await {
            for action in client_api.process(event) {
                match action {
                    Action::SendToWidget(msg) => {
                        self.to_widget_tx.send(msg).await.map_err(|_| ())?
                    }
                    Action::AcquirePermissions(cmd) => {
                        let result = cmd.result(Err("not implemented".into()));
                        events_tx.send(Event::PermissionsAcquired(result)).map_err(|_| ())?;
                    }
                    Action::GetOpenId(cmd) => {
                        let result = cmd.result(Err("not implemented".into()));
                        events_tx.send(Event::OpenIdReceived(result)).map_err(|_| ())?;
                    }
                    Action::ReadMatrixEvent(cmd) => {
                        let result = cmd.result(Err("not implemented".into()));
                        events_tx.send(Event::MatrixEventRead(result)).map_err(|_| ())?;
                    }
                    Action::SendMatrixEvent(cmd) => {
                        let result = cmd.result(Err("not implemented".into()));
                        events_tx.send(Event::MatrixEventSent(result)).map_err(|_| ())?;
                    }
                    Action::Subscribe => {}
                    Action::Unsubscribe => {}
                }
            }
        }

        Ok(())
    }
}
