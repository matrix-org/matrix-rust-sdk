//! Events that are used by the Widget API.

use std::fmt::Display;

use serde::{Deserialize, Serialize};

pub(crate) use self::{
    actions::{from_widget, to_widget, Action, Empty, ErrorBody, MessageKind, Request},
    openid::{Request as OpenIdRequest, Response as OpenIdResponse, State as OpenIdState},
};

mod actions;
mod openid;

// Usable widget Message
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

// Fallback message
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub(crate) enum BadMessage {
    WithKnownFields(BadMessageKnownFields),
    Unknown(serde_json::Value),
}
impl Display for BadMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BadMessage::WithKnownFields(_) => {
                write!(
                    f,
                    "The requested action is not supported or the data/response is malformatted:"
                )
            }
            BadMessage::Unknown(_) => {
                write!(
                    f,
                    "The message could be parsed but is unknown or is missing one of the mandatory filds: request_id, widget_id, api, action, data, response"
                )
            }
        }
    }
}
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub(crate) struct BadMessageKnownFields {
    request_id: String,
    widget_id: String,
    api: String,
    action: Option<String>,
    data: Option<serde_json::Value>,
    response: Option<serde_json::Value>,
}
