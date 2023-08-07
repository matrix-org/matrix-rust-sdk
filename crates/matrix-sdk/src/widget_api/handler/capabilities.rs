use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::widget_api::{
    messages::{
        capabilities::{Filter, Options},
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
#[derive(Default)]
pub struct Capabilities {
    // Room and state events use the same sender, reader, listener
    // on the rust-sdk side room and state events dont make a difference for the transport.
    // It is the widgets responsibility to differenciate and react to them accordingly.
    pub event_listener: Option<UnboundedReceiver<MatrixEvent>>,

    pub event_reader: Option<Box<dyn EventReader>>,
    pub event_sender: Option<Box<dyn EventSender>>,
    // TODO implement to_device_sender (not required for EC->EX)
    // pub to_device_sender: Option<Box<dyn ToDeviceSender>>,
}

#[async_trait]
pub trait EventReader {
    async fn read(&self, req: ReadEventRequest) -> Result<ReadEventResponse>;
    fn get_filter(&self) -> &Vec<Filter>;
}

#[async_trait]
pub trait EventSender {
    async fn send(&self, req: SendEventRequest) -> Result<SendEventResponse>;
    fn get_filter(&self) -> &Vec<Filter>;
}

#[async_trait]
pub trait ToDeviceSender {
    async fn send(&self, req: SendToDeviceRequest) -> Result<()>;
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
