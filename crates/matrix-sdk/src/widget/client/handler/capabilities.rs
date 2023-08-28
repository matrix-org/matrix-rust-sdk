use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedReceiver;

use super::Result;
use crate::widget::{
    client::matrix::EventServerProxy,
    messages::{
        from_widget::{ReadEventRequest, ReadEventResponse, SendEventRequest, SendEventResponse},
        MatrixEvent,
    },
    EventFilter, Permissions,
};

#[allow(missing_debug_implementations)]
#[derive(Default)]
pub struct Capabilities {
    pub listener: Option<UnboundedReceiver<MatrixEvent>>,
    pub reader: Option<EventServerProxy>,
    pub sender: Option<EventServerProxy>,
}

#[async_trait]
pub trait EventReader: Filtered + Send + Sync {
    async fn read(&self, req: ReadEventRequest) -> Result<ReadEventResponse>;
}

#[async_trait]
pub trait EventSender: Filtered + Send + Sync {
    async fn send(&self, req: SendEventRequest) -> Result<SendEventResponse>;
}

pub trait Filtered {
    fn filters(&self) -> &[EventFilter];
}

impl<'t> From<&'t Capabilities> for Permissions {
    fn from(c: &'t Capabilities) -> Self {
        Self {
            send: c.sender.as_ref().map(|e| e.filters().to_owned()).unwrap_or_default(),
            read: c.reader.as_ref().map(|e| e.filters().to_owned()).unwrap_or_default(),
        }
    }
}
