use async_trait::async_trait;

use super::{
    messages::{
        capabilities::{EventFilter, Options},
        MatrixEvent, from_widget::{SendEventResponse, SendToDeviceRequest},
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
pub trait EventReader {
    async fn read(&mut self, req: ReadEventRequest) -> Result<Vec<MatrixEvent>>;
    fn filter(&self) -> Vec<EventFilter>;
}

#[derive(Debug, Clone)]
pub struct ReadEventRequest {
    pub limit: usize,
    pub description: EventDescription,
}

#[async_trait]
pub trait EventWriter {
    async fn write(&mut self, req: SendEventRequest) -> Result<SendEventResponse>;
    fn filter(&self) -> Vec<EventFilter>;
}

#[derive(Debug, Clone)]
pub struct SendEventRequest {
    pub body: MatrixEvent,
    pub description: EventDescription,
}

#[async_trait]
pub trait ToDeviceSender {
    async fn send(&mut self, req: SendToDeviceRequest) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct EventDescription {
    pub event_type: String,
    pub kind: EventKind,
}

#[derive(Debug, Clone)]
pub enum EventKind {
    State { key: String },
    Timeline,
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
