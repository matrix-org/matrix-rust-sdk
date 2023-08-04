use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::from_str as from_json;
use tokio::sync::{
    mpsc::{UnboundedReceiver as Receiver, UnboundedSender as Sender},
    oneshot,
};
use uuid::Uuid;

use super::{
    handler::{Driver, Incoming, MessageHandler, OutgoingMessage, Request},
    messages::{
        from_widget::{
            FromWidgetMessage as FromWidgetAction, ReadEventRequest, ReadEventResponse,
            SendEventRequest, SendToDeviceRequest,
        },
        openid,
        to_widget::{
            CapabilitiesUpdatedRequest, SendMeCapabilitiesResponse,
            ToWidgetMessage as ToWidgetAction,
        },
        Header, Message as WidgetMessage, MessageBody, Response as ResponseBody,
    },
    Error, Result,
};

pub struct Transport {
    incoming: Receiver<String>,
    outgoing: Sender<WidgetMessage>,
}

impl Transport {
    pub async fn run(self) {
        let Transport { mut incoming, mut outgoing } = self;

        while let Some(raw) = incoming.recv().await {
            // if let Err(err) = process(raw).await {
            //     // TODO: We must send a properly formatted error here, but we don't have a type for it yet!
            //     // outgoing.send(Message::Error(e))
            // }
        }
    }
}

macro_rules! handle_incoming {
    {
        chain = { $action:ident($msg:ident) -> $handler:ident -> $resp:ident },
        $($req_type:ident ($req_expr:expr) -> $incoming_type:ident ($resp_expr:expr) ),* ,
    } => {
        match $action {
            $(
                FromWidgetAction::$req_type(mut $msg) => {
                    if $msg.response.is_some() {
                        return Err(Error::InvalidJSON);
                    }

                    let (req, resp) = Request::new($req_expr);
                    $handler.handle(Incoming::$incoming_type(req)).await?;
                    let response = match resp.receiver.await.expect("Bug: handler died") {
                        Ok($resp) => ResponseBody::Response($resp_expr),
                        Err(err) => ResponseBody::error(err.to_string()),
                    };

                    $msg.response = Some(response);
                    FromWidgetAction::$req_type($msg)
                }
            )*
        }
    }
}

async fn send<T: OutgoingMessage>(
    msg: T,
    sink: Sender<ToWidgetAction>,
    widget_id: String,
    map: Arc<Mutex<HashMap<String, oneshot::Sender<ToWidgetAction>>>>,
) -> Result<T::Response> {
    let id = Uuid::new_v4();
    let raw = msg.into_message(Header { request_id: id.to_string(), widget_id });
    sink.send(raw).map_err(|_| Error::WidgetDied)?;

    let (tx, rx) = oneshot::channel();
    map.lock().unwrap().insert(id.to_string(), tx);
    Ok(T::extract_response(rx.await.map_err(|_| Error::WidgetDied)?)?)
}

async fn process<T: Driver>(
    raw: String,
    mut handler: MessageHandler<T>,
    outgoing: Sender<WidgetMessage>,
    map: Arc<Mutex<HashMap<String, oneshot::Sender<ToWidgetAction>>>>,
) -> Result<()> {
    let decoded: WidgetMessage = from_json(&raw).map_err(|_| Error::InvalidJSON)?;

    match decoded {
        WidgetMessage::ToWidget(to_widget) => {
            let pending =
                map.lock().unwrap().remove(to_widget.id()).ok_or(Error::UnexpectedResponse)?;
            pending.send(to_widget).map_err(|_| Error::WidgetDied).expect("Handler died");
        }

        WidgetMessage::FromWidget(from_widget) => {
            let response = handle_incoming! {
                chain = { from_widget(msg) -> handler -> resp },

                GetSupportedApiVersion (msg.request)
                    -> GetSupportedApiVersion (resp),

                ContentLoaded (msg.request)
                    -> ContentLoaded (resp),

                GetOpenId (openid::Request{ id: msg.header.request_id.clone() })
                    -> GetOpenID (resp),

                SendToDevice (msg.request.clone())
                    -> SendToDeviceRequest (resp),

                SendEvent (msg.request.clone())
                    -> SendEvent (resp),

                ReadEvent (msg.request.clone().into())
                    -> ReadEvents (ReadEventResponse { events: resp }),

                ReadRelations (msg.request.clone().into())
                    -> ReadEvents (ReadEventResponse { events: resp }),
            };

            outgoing.send(WidgetMessage::FromWidget(response)).map_err(|_| Error::WidgetDied)?;
        }
    }

    Ok(())
}
