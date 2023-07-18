use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Header {
    pub request_id: String,
    pub widget_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Message<Req, Resp> {
    #[serde(flatten)]
    pub header: Header,
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
    pub error: WidgetErrorMessage
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WidgetErrorMessage {
    pub message: String
}
