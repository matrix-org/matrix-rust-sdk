use async_trait::async_trait;
use serde_json::from_str as from_json;
use tokio::sync::mpsc::{UnboundedReceiver as Receiver, UnboundedSender as Sender};

use super::{
    handler::{Driver, Incoming, MessageHandler, Request},
    messages::{
        from_widget::{
            FromWidgetMessage as FromWidgetAction, ReadEventRequest, SendEventRequest,
            SendToDeviceRequest, ReadEventResponse,
        },
        to_widget::{
            CapabilitiesUpdatedRequest, SendMeCapabilitiesResponse,
            ToWidgetMessage as ToWidgetAction,
        },
        openid,
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
        from_widget = { $action:expr }
        env = { $handler:expr, $outgoing:expr },
        identifiers = { $msg:ident, $resp:ident },
        $($req_type:ident => {$incoming_type:ident { $req_expr:expr } -> { $resp_expr:expr }}),* ,
    } => {
        match $action {
            $(
                FromWidgetAction::$req_type(mut $msg) => {
                    $msg.response.as_ref().ok_or(Error::InvalidJSON)?;

                    let (req, resp) = Request::new($req_expr);
                    $handler.handle(Incoming::$incoming_type(req)).await?;
                    let response = match resp.receiver.await.expect("Bug: handler died") {
                        Ok($resp) => ResponseBody::Response($resp_expr),
                        Err(err) => ResponseBody::error(err.to_string()),
                    };

                    $msg.response = Some(response);
                    $outgoing
                        .send(WidgetMessage::FromWidget(FromWidgetAction::$req_type($msg)))
                        .map_err(|_| Error::WidgetDied)?;
                }
            )*
        }
    }
}

async fn process<T: Driver>(
    raw: String,
    mut handler: MessageHandler<T>,
    outgoing: Sender<WidgetMessage>,
) -> Result<()> {
    let decoded: WidgetMessage = from_json(&raw).map_err(|_| Error::InvalidJSON)?;

    match decoded {
        WidgetMessage::ToWidget(to_widget) => {
            todo!()
        }

        WidgetMessage::FromWidget(from_widget) => {
            handle_incoming! {
                from_widget = { from_widget }
                env = {  handler, outgoing },
                identifiers = { msg, resp },

                GetSupportedApiVersion => {
                    GetSupportedApiVersion { msg.request } -> { resp }
                },

                ContentLoaded => {
                    ContentLoaded { msg.request } -> { resp }
                },

                GetOpenId => {
                    GetOpenID { openid::Request{ id: msg.header.request_id.clone() } } -> { resp }
                },

                SendToDevice => {
                    SendToDeviceRequest { msg.request.clone() } -> { resp }
                },

                SendEvent => {
                    SendEvent { msg.request.clone() } -> { resp }
                },

                ReadEvent => {
                    ReadEvents { msg.request.clone().into() } -> { ReadEventResponse { events: resp }}
                },

                ReadRelations => {
                    ReadEvents { msg.request.clone().into() } -> { ReadEventResponse { events: resp }}
                },
            }
        }
    }

    Ok(())
}
