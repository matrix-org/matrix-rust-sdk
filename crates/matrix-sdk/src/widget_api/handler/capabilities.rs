use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::widget_api::{
    messages::{
        capabilities::{Filter, Options},
        from_widget::{ReadEventRequest, ReadEventResponse, SendEventRequest, SendEventResponse},
        MatrixEvent,
    },
    Result,
};

/// A wrapper for the matrix client that only exposes what is available through the capabilities.
#[allow(missing_debug_implementations)]
#[derive(Default)]
pub struct Capabilities {
    pub listener: Option<UnboundedReceiver<MatrixEvent>>,
    pub reader: Option<Box<dyn EventReader>>,
    pub sender: Option<Box<dyn EventSender>>,
}

#[async_trait]
pub trait EventReader: Filtered + Send {
    async fn read(&self, req: ReadEventRequest) -> Result<ReadEventResponse>;
}

#[async_trait]
pub trait EventSender: Filtered + Send {
    async fn send(&self, req: SendEventRequest) -> Result<SendEventResponse>;
}

pub trait Filtered {
    fn filters(&self) -> &[Filter];
}

impl<'t> From<&'t Capabilities> for Options {
    fn from(c: &'t Capabilities) -> Self {
        Self {
            send_filter: c.sender.as_ref().map(|e| e.filters().to_owned()).unwrap_or_default(),
            read_filter: c.reader.as_ref().map(|e| e.filters().to_owned()).unwrap_or_default(),
            ..Options::default()
        }
    }
}
