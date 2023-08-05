use crate::widget_api::messages::{capabilities::Options, openid::State as OpenIDState, to_widget::{ToWidgetMessage, CapabilitiesUpdatedRequest}, MessageBody, Header, Response};

use super::{Result, Error};

pub trait OutgoingMessage: Sized + Send + Sync + 'static {
    type Response;

    fn into_message(self, header: Header) -> ToWidgetMessage;
    fn extract_response(message: ToWidgetMessage) -> Result<Self::Response>;
}

pub struct SendMeCapabilities;

impl OutgoingMessage for SendMeCapabilities {
    type Response = Options;

    fn into_message(self, header: Header) -> ToWidgetMessage {
        ToWidgetMessage::SendMeCapabilities(MessageBody {
            header,
            request: (),
            response: None,
        })
    }

    fn extract_response(msg: ToWidgetMessage) -> Result<Self::Response> {
        match msg {
            ToWidgetMessage::SendMeCapabilities(MessageBody { response, .. }) => {
                match response.ok_or(Error::InvalidJSON)? {
                    Response::Response(r) => Ok(r.capabilities),
                    Response::Error(e) => Err(Error::WidgetError(e.error.message)),
                }
            }
            _ => Err(Error::UnexpectedResponse),
        }
    }
}

pub struct CapabilitiesUpdated {
    pub requested: Options,
    pub approved: Options,
}

impl OutgoingMessage for CapabilitiesUpdated {
    type Response = ();

    fn into_message(self, header: Header) -> ToWidgetMessage {
        ToWidgetMessage::CapabilitiesUpdated(MessageBody {
            header,
            request: CapabilitiesUpdatedRequest {
                requested: self.requested,
                approved: self.approved,
            },
            response: None,
        })
    }

    fn extract_response(msg: ToWidgetMessage) -> Result<Self::Response> {
        match msg {
            ToWidgetMessage::CapabilitiesUpdated(MessageBody { response, .. }) => {
                match response.ok_or(Error::InvalidJSON)? {
                    Response::Response(r) => Ok(r),
                    Response::Error(e) => Err(Error::WidgetError(e.error.message)),
                }
            }
            _ => Err(Error::UnexpectedResponse),
        }
    }
}

pub struct OpenIDUpdated(pub OpenIDState);

impl OutgoingMessage for OpenIDUpdated {
    type Response = ();

    fn into_message(self, header: Header) -> ToWidgetMessage {
        ToWidgetMessage::OpenIdCredentials(MessageBody {
            header,
            request: self.0,
            response: None,
        })
    }

    fn extract_response(msg: ToWidgetMessage) -> Result<Self::Response> {
        match msg {
            ToWidgetMessage::OpenIdCredentials(MessageBody { response, .. }) => {
                match response.ok_or(Error::InvalidJSON)? {
                    Response::Response(r) => Ok(r),
                    Response::Error(e) => Err(Error::WidgetError(e.error.message)),
                }
            }
            _ => Err(Error::UnexpectedResponse),
        }
    }
}
