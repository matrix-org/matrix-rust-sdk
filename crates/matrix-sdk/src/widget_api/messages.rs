use serde::{Deserialize, Serialize};

use super::{capabilities::Options, handler::ApiVersion};

// #[derive(Serialize, Deserialize, Debug)]
// #[serde(tag = "api")]
// pub enum JsonMessage {
//     #[serde(rename = "fromWidget")]
//     FromWidget(FromWidget),
//     #[serde(rename = "toWidget")]
//     ToWidget(ToWidget),
// }

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "action")]
pub enum ToWidget {
    // Requests.
    #[serde(rename = "capabilities")]
    SendMeCapabilities,
    #[serde(rename = "notify_capabilities")]
    CapabilitiesUpdated,

    // Responses.
    #[serde(rename = "supported_api_versions")]
    SupportedApiVersions(Message<Vec<ApiVersion>, ()>),
    #[serde(rename = "content_loaded")]
    ContentLoaded(Message<(), ()>),
    #[serde(rename = "org.matrix.msc2931.navigate")]
    Navigate(Message<String, ()>),
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "action")]
pub enum FromWidget {
    // Requests.
    #[serde(rename = "supported_api_versions")]
    SupportedApiVersions(Message<(), ()>),
    #[serde(rename = "content_loaded")]
    ContentLoaded(Message<(), ()>),
    #[serde(rename = "org.matrix.msc2931.navigate")]
    Navigate(Message<String, ()>),

    // Responses.
    #[serde(rename = "capabilities")]
    Capabilities(Message<(), Options>),
    #[serde(rename = "notify_capabilities")]
    CapabilitiesConfirmed(Message<(), ()>),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Message<Req, Resp> {
    #[serde(flatten)]
    pub header: Header,
    #[serde(rename = "data")]
    pub request: Req,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Response<Resp>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Header {
    pub request_id: String,
    pub widget_id: String,
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
