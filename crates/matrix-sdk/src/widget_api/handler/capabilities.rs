use async_trait::async_trait;
use tokio::sync::mpsc::Receiver;

use crate::widget_api::{
    messages::{
        capabilities::{EventFilter, Options},
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
    filter_send_room_event: Vec<EventFilter>,
    filter_read_room_event: Vec<EventFilter>,
    filter_send_state_event: Vec<EventFilter>,
    filter_read_state_event: Vec<EventFilter>,

    // Room and state events use the same sender, reader, listener
    // on the rust-sdk side room and state events dont make a difference for the transport. 
    // It is the widgets responsibility to differenciate and react to them accordingly.
    pub event_listener: Option<Receiver<MatrixEvent>>,

    pub event_reader: Option<Box<dyn EventReader>>,
    pub event_sender: Option<Box<dyn EventSender>>,

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
        Options {
            screenshot: false,

            // room events
            send_room_event: capabilities.filter_send_room_event.clone(),
            read_room_event: capabilities.filter_read_room_event.clone(),
            // state events
            send_state_event: capabilities.filter_send_state_event.clone(),
            read_state_event: capabilities.filter_read_state_event.clone(),

            always_on_screen: false, // "m.always_on_screen",

            requires_client: false,
        }
    }
}
