use async_trait::async_trait;
use tokio::sync::mpsc::Receiver;

use crate::widget_api::{
    messages::{
        capabilities::Options,
        from_widget::{
            ReadEventRequest, ReadEventResponse, SendEventRequest, SendEventResponse,
            SendToDeviceRequest,
        },
        MatrixEvent,
    },
    Result,
};

/// A wrapper for the matrix client that only exposes what is available through the capabilities.
#[allow(missing_debug_implementations)]
pub struct Capabilities {
    // The options are a private member,
    // they contain the type filters for the listeners.
    options: Options,

    pub room_event_listener: Option<Receiver<MatrixEvent>>,
    pub state_event_listener: Option<Receiver<MatrixEvent>>,

    pub room_event_reader: Option<Box<dyn EventReader>>,
    pub room_event_sender: Option<Box<dyn EventSender>>,
    pub state_event_reader: Option<Box<dyn EventReader>>,
    pub state_event_sender: Option<Box<dyn EventSender>>,

    pub to_device_sender: Option<Box<dyn ToDeviceSender>>,
}

#[async_trait]
pub trait EventReader {
    async fn read(&mut self, req: ReadEventRequest) -> Result<ReadEventResponse>;
}

#[async_trait]
pub trait EventSender {
    async fn send(&mut self, req: SendEventRequest) -> Result<SendEventResponse>;
}

#[async_trait]
pub trait ToDeviceSender {
    async fn send(&mut self, req: SendToDeviceRequest) -> Result<()>;
}

impl<'t> From<&'t Capabilities> for Options {
    fn from(capabilities: &'t Capabilities) -> Self {
        capabilities.options.clone()
    }
}
