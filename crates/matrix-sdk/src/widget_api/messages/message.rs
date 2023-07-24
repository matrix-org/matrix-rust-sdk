use serde::{Deserialize, Serialize};

use super::{FromWidgetMessage, ToWidgetMessage};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "api")]
pub enum Message {
    #[serde(rename = "fromWidget")]
    FromWidget(FromWidgetMessage),
    #[serde(rename = "toWidget")]
    ToWidget(ToWidgetMessage),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MessageBody<Req, Resp> {
    pub request_id: String,
    pub widget_id: String,
    #[serde(rename = "data")]
    pub request: Req,
    pub response: Option<Response<Resp>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum Response<Resp> {
    Error(WidgetError),
    Response(Resp),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WidgetError {
    pub error: WidgetErrorMessage,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WidgetErrorMessage {
    pub message: String,
}

impl<Resp> Into<Result<Resp, WidgetError>> for Response<Resp> {
    fn into(self) -> Result<Resp, WidgetError> {
        match self {
            Response::Error(err) => Err(err),
            Response::Response(resp) => Ok(resp),
        }
    }
}
