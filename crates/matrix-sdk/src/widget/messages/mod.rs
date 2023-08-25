//! Events that are used by the Widget API.

use serde::{Deserialize, Serialize};

pub use self::{
    actions::{from_widget, to_widget, Action, Empty, MessageKind, Request, Response},
    event::{EventType, MatrixEvent},
    openid::{Request as OpenIDRequest, Response as OpenIDResponse, State as OpenIDState},
};

mod actions;
mod event;
mod openid;

#[derive(Serialize, Deserialize, Debug)]
pub struct Message {
    #[serde(flatten)]
    pub header: Header,
    pub action: Action,
}

impl Message {
    pub fn new(header: Header, action: Action) -> Self {
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
