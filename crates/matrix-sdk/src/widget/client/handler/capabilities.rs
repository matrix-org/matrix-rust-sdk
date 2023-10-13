//! Capabilities that are accessible to the message handler once negotiation is
//! complete. Capabilities must represent the functionatliy of the negotiated
//! capabilities.

use ruma::{events::AnyTimelineEvent, serde::Raw};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::widget::{client::matrix::EventServerProxy, Permissions};

/// Client-side capabilities from the perspective of the message handler (our
/// client side widget api state machine) that we can rely upon when processing
/// incomiong requests from a widget. Essentially it provides a confined
/// high-level access to the matrix driver that respects the negotiated
/// capabilities (i.e. the creator of `Capabilities` will ensure that the actual
/// implementation respects negotiated capabilities / permissions).
#[allow(missing_debug_implementations)]
#[derive(Default)]
pub struct Capabilities {
    /// Receiver of incoming matrix events. `None` if we don't have permissions
    /// to subscribe to the new events.
    pub listener: Option<UnboundedReceiver<Raw<AnyTimelineEvent>>>,
    /// Event reader (allows reading events). `None` if reading is forbidden.
    pub reader: Option<EventServerProxy>,
    /// Event sender (allows sending events). `None` if sending is forbidden.
    pub sender: Option<EventServerProxy>,
}

/// Constructs `Permissions` from existing `Capabilities`. Since
/// `Capabilities` represent negotiated `Permissions`, we can always construct
/// `Permissions` from existing `Capabilities`.
impl<'t> From<&'t Capabilities> for Permissions {
    fn from(c: &'t Capabilities) -> Self {
        Self {
            send: c.sender.as_ref().map(|e| e.filters().to_owned()).unwrap_or_default(),
            read: c.reader.as_ref().map(|e| e.filters().to_owned()).unwrap_or_default(),
            ..Self::default()
        }
    }
}
