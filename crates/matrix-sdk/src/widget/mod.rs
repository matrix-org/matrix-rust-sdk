//! Widget API implementation.

//#![warn(unreachable_pub)]

use async_channel::{Receiver, Sender};

use self::client::{run as client_widget_api, MatrixDriver, Result};
use crate::room::Room;

mod client;
mod filter;
mod messages;
mod permissions;

pub use self::{
    filter::{EventFilter, MessageLikeEventFilter, StateEventFilter},
    permissions::{Permissions, PermissionsProvider},
};

/// Describes a widget.
#[derive(Debug)]
pub struct Widget {
    /// Settings for the widget.
    pub settings: WidgetSettings,
    /// Communication channels with a widget.
    pub comm: Comm,
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

/// Runs client widget API for a given `widget` with a given
/// `permission_manager` within a given `room`. The function returns once the
/// API is completed (the widget disconnected etc).
pub async fn run_client_widget_api(
    widget: Widget,
    permission_manager: impl PermissionsProvider,
    room: Room,
) -> Result<()> {
    // TODO: define a cancellation mechanism (?).
    client_widget_api(MatrixDriver::new(room, permission_manager), widget).await
}
