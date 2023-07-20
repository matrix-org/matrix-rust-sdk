use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Message<Req, Resp> {
    pub request_id: String,
    pub widget_id: String,
    #[serde(rename = "data")]
    pub request: Req,
    pub response: Option<Response<Resp>>,
}

#[derive(Debug, Serialize, Deserialize )]
#[serde(untagged)]
pub enum Response<Resp> {
    Error(WidgetError),
    Response(Resp),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WidgetError {
    pub error: WidgetErrorMessage,
}

#[derive(Debug, Serialize, Deserialize)]
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
