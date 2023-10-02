// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use serde::{Deserialize, Serialize};

pub use self::openid::{OpenIdResponse, OpenIdState};

mod openid;
mod permissions;

pub mod from_widget;
pub mod to_widget;

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct IncomingMessage {
    #[serde(flatten)]
    pub(crate) header: Header,
    #[serde(flatten)]
    pub(crate) kind: IncomingMessageKind,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "api")]
#[serde(rename_all = "camelCase")]
pub(crate) enum IncomingMessageKind {
    FromWidget(from_widget::RequestType),
    ToWidget(to_widget::ResponseType),
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct OutgoingMessage {
    #[serde(flatten)]
    pub(crate) header: Header,
    #[serde(flatten)]
    pub(crate) kind: OutgoingMessageKind,
}

impl OutgoingMessage {
    pub fn response(header: Header, response: from_widget::ResponseType) -> Self {
        Self { header, kind: OutgoingMessageKind::FromWidget(response) }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "api")]
#[serde(rename_all = "camelCase")]
pub(crate) enum OutgoingMessageKind {
    FromWidget(from_widget::ResponseType),
    #[allow(dead_code)]
    ToWidget(to_widget::RequestType),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Header {
    pub request_id: String,
    pub widget_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Request<T> {
    #[serde(rename = "data")]
    pub content: T,
}

impl<T> Request<T> {
    pub fn map<R>(self, response: Result<R, String>) -> Response<T, R> {
        Response {
            request: self.content,
            response: match response {
                Ok(response) => ResponseBody::Success(response),
                Err(error) => ResponseBody::Failure(ErrorBody::new(error)),
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Response<Req, Resp> {
    #[serde(rename = "data")]
    pub request: Req,
    pub response: ResponseBody<Resp>,
}

impl<Req, Resp: Clone> Response<Req, Resp> {
    #[allow(dead_code)]
    pub fn result(&self) -> Result<Resp, String> {
        match &self.response {
            ResponseBody::Success(resp) => Ok(resp.clone()),
            ResponseBody::Failure(err) => Err(err.as_ref().to_owned()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ResponseBody<T> {
    Success(T),
    Failure(ErrorBody),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ErrorBody {
    pub error: ErrorContent,
}

impl ErrorBody {
    pub fn new(message: impl AsRef<str>) -> Self {
        Self { error: ErrorContent { message: message.as_ref().to_owned() } }
    }
}

impl AsRef<str> for ErrorBody {
    fn as_ref(&self) -> &str {
        &self.error.message
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ErrorContent {
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Empty {}
