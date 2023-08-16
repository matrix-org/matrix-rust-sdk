// TODO: Remove this supress once we're ready to write the documentation.
#![allow(missing_docs)]

pub mod api;
pub mod error;
pub mod handler;
pub mod matrix;
pub mod messages;

pub use self::{
    api::{run, widget::Widget},
    error::{Error, Result},
    matrix::{Driver as MatrixDriver, PermissionProvider},
};
use crate::room::Joined as JoinedRoom;

/// Runs client widget API for a given `widget` with a given `permission_manager` within a given `room`.
/// The function returns once the API is completed (the widget disconnected etc).
pub async fn run_client_widget_api(
    widget: Widget,
    permission_manager: impl PermissionProvider,
    room: JoinedRoom,
) -> Result<()> {
    run(MatrixDriver::new(room, permission_manager), widget).await
}
