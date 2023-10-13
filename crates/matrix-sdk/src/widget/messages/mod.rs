//! Events that are used by the Widget API.

use serde::{Deserialize, Serialize};

pub(crate) use self::actions::{
    from_widget, to_widget, Action, Empty, ErrorBody, MessageKind, Request,
};

mod actions;
pub(crate) mod openid;

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Message {
    #[serde(flatten)]
    pub header: Header,
    #[serde(flatten)]
    pub(crate) action: Action,
}

impl Message {
    pub(crate) fn new(header: Header, action: Action) -> Self {
        Self { header, action }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    pub request_id: String,
    pub widget_id: String,
}

impl Header {
    pub fn new(request_id: impl Into<String>, widget_id: impl Into<String>) -> Self {
        Self { request_id: request_id.into(), widget_id: widget_id.into() }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ErrorMessage {
    #[serde(flatten)]
    pub original_request: Option<serde_json::Value>,
    pub response: ErrorBody,
}
