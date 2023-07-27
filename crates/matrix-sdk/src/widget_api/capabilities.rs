
use super::messages::{MatrixEvent, self};

/// A wrapper for the matrix client that only exposes what is available through the capabilities.
#[allow(missing_debug_implementations)]
pub struct Capabilities {
    pub send_room_event: Option<Box<dyn Fn(MatrixEvent) + Send + Sync + 'static>>,
}

impl<'t> From<&'t Capabilities> for messages::capabilities::Options {
    fn from(capabilities: &'t Capabilities) -> Self {
        Self {
            send_room_event: if capabilities.send_room_event.is_some() {
                Some(vec![])
            } else {
                None
            },
            ..Default::default()
        }
    }
}
