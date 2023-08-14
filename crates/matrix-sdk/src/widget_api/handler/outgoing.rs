use crate::widget_api::messages::{
    capabilities::Options,
    openid::State as OpenIDState,
    to_widget::{CapabilitiesUpdatedRequest, ToWidgetMessage},
    Header, MessageBody,
};

use super::{Error, Result};

pub trait OutgoingMessage: Sized + Send + Sync + 'static {
    type Response;

    fn into_message(self, header: Header) -> ToWidgetMessage;
    fn extract_response(message: ToWidgetMessage) -> Result<Self::Response>;
}

pub struct SendMeCapabilities;
impl OutgoingMessage for SendMeCapabilities {
    type Response = Options;

    fn into_message(self, header: Header) -> ToWidgetMessage {
        ToWidgetMessage::SendMeCapabilities(MessageBody::request(header, ()))
    }

    fn extract_response(msg: ToWidgetMessage) -> Result<Self::Response> {
        match msg {
            ToWidgetMessage::SendMeCapabilities(body) => Ok(body.response()?.capabilities),
            _ => Err(Error::UnexpectedResponse),
        }
    }
}

pub struct CapabilitiesUpdated(pub CapabilitiesUpdatedRequest);
impl OutgoingMessage for CapabilitiesUpdated {
    type Response = ();

    fn into_message(self, header: Header) -> ToWidgetMessage {
        ToWidgetMessage::CapabilitiesUpdated(MessageBody::request(header, self.0))
    }

    fn extract_response(msg: ToWidgetMessage) -> Result<Self::Response> {
        match msg {
            ToWidgetMessage::CapabilitiesUpdated(body) => Ok(body.response()?),
            _ => Err(Error::UnexpectedResponse),
        }
    }
}

pub struct OpenIDUpdated(pub OpenIDState);
impl OutgoingMessage for OpenIDUpdated {
    type Response = ();

    fn into_message(self, header: Header) -> ToWidgetMessage {
        ToWidgetMessage::OpenIdCredentials(MessageBody::request(header, self.0))
    }

    fn extract_response(msg: ToWidgetMessage) -> Result<Self::Response> {
        match msg {
            ToWidgetMessage::OpenIdCredentials(body) => Ok(body.response()?),
            _ => Err(Error::UnexpectedResponse),
        }
    }
}
