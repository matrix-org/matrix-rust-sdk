use crate::widget_api::{
    messages::from_widget::{SendEventResponse, SendEventRequest, SendToDeviceRequest},
};

use super::{
    super::{
        capabilities::ReadEventRequest,
        messages::{openid, MatrixEvent, SupportedVersions},
    },
    Request,
};

#[allow(missing_debug_implementations)]
pub enum Message {
    GetSupportedApiVersion(Request<(), SupportedVersions>),
    ContentLoaded(Request<(), ()>),
    GetOpenID(Request<openid::Request, openid::State>),
    ReadEvents(Request<ReadEventRequest, Vec<MatrixEvent>>),
    SendEvent(Request<SendEventRequest, SendEventResponse>),
    SendToDeviceRequest(Request<SendToDeviceRequest, ()>),
}
