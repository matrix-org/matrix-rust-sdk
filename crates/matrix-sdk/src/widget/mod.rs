//! Client widget API implementation.

use async_channel::{Receiver, Sender};

use crate::room::Room as JoinedRoom;

mod permissions;

pub use self::permissions::{EventFilter, Permissions, PermissionsProvider};

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
    pub from: Receiver<String>,
    /// Raw outgoing messages from the client (SDK) to the widget (normally
    /// formatted as JSON).
    pub to: Sender<String>,
}

/// Starts a client widget API state machine for a given `widget` in a given
/// joined `room`. The function returns once the widget is disconnected or any
/// terminal error occurs.
///
/// Not implemented yet, currently always panics.
pub async fn run_widget_api(
    _room: JoinedRoom,
    _widget: Widget,
    _permissions_provider: impl PermissionsProvider,
) -> Result<(), ()> {
    Err(())
}
