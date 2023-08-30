use ruma::{events::AnySyncTimelineEvent, serde::Raw};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::widget::{client::matrix::EventServerProxy, Permissions};

#[allow(missing_debug_implementations)]
#[derive(Default)]
pub struct Capabilities {
    pub listener: Option<UnboundedReceiver<Raw<AnySyncTimelineEvent>>>,
    pub reader: Option<EventServerProxy>,
    pub sender: Option<EventServerProxy>,
}

impl<'t> From<&'t Capabilities> for Permissions {
    fn from(c: &'t Capabilities) -> Self {
        Self {
            send: c.sender.as_ref().map(|e| e.filters().to_owned()).unwrap_or_default(),
            read: c.reader.as_ref().map(|e| e.filters().to_owned()).unwrap_or_default(),
            ..Self::default()
        }
    }
}
