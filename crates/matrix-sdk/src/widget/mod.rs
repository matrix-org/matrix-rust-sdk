//! Widget API implementation.

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use self::client::{run as client_widget_api, MatrixDriver, Result};
pub use self::permissions::{EventFilter, Permissions, PermissionsProvider};
use crate::room::Room as JoinedRoom;

mod client;
mod messages;
mod permissions;

/// Describes a widget.
#[derive(Debug)]
pub struct Widget {
    /// Information about the widget.
    pub info: Info,
    /// Communication channels with a widget.
    pub comm: Comm,
}

/// Information about a widget.
#[derive(Debug)]
pub struct Info {
    /// Widget's unique identifier.
    pub id: String,
    /// Whether or not the widget should be initialized on load message
    /// (`ContentLoad` message), or upon creation/attaching of the widget to
    /// the SDK's state machine that drives the API.
    pub init_on_load: bool,
}

/// Communication "pipes" with a widget.
#[derive(Debug)]
pub struct Comm {
    /// Raw incoming messages from the widget (normally, formatted as JSON).
    pub from: UnboundedReceiver<String>,
    /// Raw outgoing messages from the client (SDK) to the widget (normally
    /// formatted as JSON).
    pub to: UnboundedSender<String>,
}

/// Runs client widget API for a given `widget` with a given
/// `permission_manager` within a given `room`. The function returns once the
/// API is completed (the widget disconnected etc).
pub async fn run_client_widget_api(
    widget: Widget,
    permission_manager: impl PermissionsProvider,
    room: JoinedRoom,
) -> Result<()> {
    // TODO: define a cancellation mechanism (?).
    client_widget_api(MatrixDriver::new(room, permission_manager), widget).await
}
