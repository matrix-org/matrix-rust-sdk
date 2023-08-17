//! Client widget API implementation.

use crate::room::Room as JoinedRoom;

pub mod permissions;
pub mod widget;

/// Starts a client widget API state machine for a given `widget` in a given joined `room`.
/// The function returns once the widget is disconnected or any terminal error occurs.
pub async fn run_widget_api(
    _room: JoinedRoom,
    _widget: widget::Widget,
    _permissions_provider: impl permissions::PermissionsProvider,
) -> Result<(), ()> {
    todo!()
}
