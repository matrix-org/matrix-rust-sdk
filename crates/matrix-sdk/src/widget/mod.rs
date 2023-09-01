//! Widget API implementation.

use async_channel::{Receiver, Sender};
use tokio::sync::mpsc::unbounded_channel;

use crate::room::Room as JoinedRoom;

mod client;
mod filter;
mod permissions;
mod widget_settings;

use crate::Room;

use self::client::{Action, ClientApi, Event};
pub use self::{
    filter::{EventFilter, MessageLikeEventFilter, StateEventFilter},
    permissions::{Permissions, PermissionsProvider},
    widget_settings::WidgetSettings,
};

/// Describes a widget.
#[derive(Debug)]
pub struct Widget {
    /// Settings for the widget.
    pub settings: WidgetSettings,
    /// Communication channels with a widget.
    pub comm: Comm,
}

/// Communication "pipes" with a widget.
#[derive(Debug)]
pub struct Comm {
    /// Raw incoming messages from the widget (normally, formatted as JSON).
    ///
    /// These can be both requests and responses. Users of this API should not
    /// care what's what though because they are only supposed to forward
    /// messages between the webview / iframe, and the SDK's widget driver.
    pub from: Receiver<String>,
    /// Raw outgoing messages from the client (SDK) to the widget (normally
    /// formatted as JSON).
    ///
    /// These can be both requests and responses. Users of this API should not
    /// care what's what though because they are only supposed to forward
    /// messages between the webview / iframe, and the SDK's widget driver.
    pub to: Sender<String>,
}

/// Starts a client widget API state machine for a given `widget` in a given
/// joined `room`. The function returns once the widget is disconnected or any
/// terminal error occurs.
///
/// Not implemented yet! Currently, it does not contain any useful
/// functionality, it only blindly forwards the messages and returns errors once
/// a non-implemented part is triggered.
pub async fn run_widget_api(
    _room: JoinedRoom,
    widget: Widget,
    _permissions_provider: impl PermissionsProvider,
) -> Result<(), ()> {
    let Comm { from, to } = widget.comm;

    // Create a channel so that we can conveniently send all events to it.
    let (events_tx, mut events_rx) = unbounded_channel();

    // Forward all incoming raw messages into events and send them to the sink.
    // Equivalent of the:
    // `from.map(|m| Ok(Event::MessageFromWidget(msg)).forward(events_tx)`,
    // but apparently `UnboundedSender<T>` does not implement `Sink<T>`.
    let tx = events_tx.clone();
    tokio::spawn(async move {
        while let Ok(msg) = from.recv().await {
            let _ = tx.send(Event::MessageFromWidget(msg));
        }
    });

    // Process events by passing them to the `ClientApi` implementation.
    let mut client_api = ClientApi::new();
    while let Some(event) = events_rx.recv().await {
        for action in client_api.process(event) {
            match action {
                Action::SendToWidget(msg) => to.send(msg).await.map_err(|_| ())?,
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
