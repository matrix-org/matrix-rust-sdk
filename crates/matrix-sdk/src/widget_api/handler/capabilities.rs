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
    pub event_listener: Option<UnboundedReceiver<MatrixEvent>>,
    pub event_reader: Option<Box<dyn EventReader>>,
    pub event_sender: Option<Box<dyn EventSender>>,
}

#[async_trait]
pub trait EventReader: Send {
    async fn read(&self, req: ReadEventRequest) -> Result<ReadEventResponse>;
    fn get_filter(&self) -> &Vec<Filter>;
}

#[async_trait]
pub trait EventSender: Send {
    async fn send(&self, req: SendEventRequest) -> Result<SendEventResponse>;
    fn get_filter(&self) -> &Vec<Filter>;
}

impl<'t> From<&'t Capabilities> for Options {
    fn from(capabilities: &'t Capabilities) -> Self {
        Options {
            // room events
            send_filter: match &capabilities.event_sender {
                None => vec![],
                Some(sender) => sender.get_filter().clone(),
            },
            read_filter: match &capabilities.event_reader {
                None => vec![],
                Some(reader) => reader.get_filter().clone(),
            },

            // all other unimplemented capabilities
            ..Options::default()
        }
    }
}
