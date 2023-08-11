use async_trait::async_trait;

use super::{
    messages::{
        capabilities::{EventFilter, Options},
        from_widget::{
            ReadEventRequest, SendEventRequest, SendEventResponse,
            SendToDeviceRequest,
        },
        MatrixEvent,
    },
    Result,
};

/// A wrapper for the matrix client that only exposes what is available through the capabilities.
#[allow(missing_debug_implementations)]
pub struct Capabilities {
    pub event_reader: Option<Box<dyn EventReader>>,
    pub event_writer: Option<Box<dyn EventWriter>>,
    pub to_device_sender: Option<Box<dyn ToDeviceSender>>,
}

#[async_trait]
pub trait EventReader: Send {
    async fn read(&mut self, req: ReadEventRequest) -> Result<Vec<MatrixEvent>>;
    fn filter(&self) -> Vec<EventFilter>;
}
#[async_trait]
pub trait EventWriter: Send {
    async fn write(&mut self, req: SendEventRequest) -> Result<SendEventResponse>;
    fn filter(&self) -> Vec<EventFilter>;
}

#[async_trait]
pub trait ToDeviceSender: Send {
    async fn send(&mut self, req: SendToDeviceRequest) -> Result<()>;
}

impl<'t> From<&'t Capabilities> for Options {
    fn from(capabilities: &'t Capabilities) -> Self {
        Self {
            send_room_event: capabilities.event_writer.as_ref().map(|w| w.filter()),
            send_state_event: capabilities.event_writer.as_ref().map(|w| w.filter()),
            receive_room_event: capabilities.event_reader.as_ref().map(|r| r.filter()),
            receive_state_event: capabilities.event_reader.as_ref().map(|r| r.filter()),
            ..Default::default()
        }
    }
}
