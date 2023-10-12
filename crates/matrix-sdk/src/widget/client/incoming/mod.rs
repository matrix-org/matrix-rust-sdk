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

//! Processing of the incoming messages.

#![allow(dead_code)]

use serde::{de, Deserialize, Deserializer};

pub(crate) use self::from_widget::{MatrixProxy, PostAction, ReadEventBody};
use super::ClientApiSettings;
use crate::widget::Permissions;

mod from_widget;

/// Various types of the incoming message.
pub(crate) enum IncomingMessage {
    /// Incoming request from a widget.
    FromWidget(Box<dyn IncomingRequest>),
}

/// Let's define a request from a widget as something that can be handled with
/// certain context. Each request returns a response and an optional action to
/// be performed by the caller (post-action that must be executed after sending
/// a response).
#[async_trait::async_trait]
pub(crate) trait IncomingRequest: Send {
    async fn handle(self: Box<Self>, ctx: Context) -> HandleResult;
}

/// Context (environment) that is required in order to process an incoming
/// request.
pub(crate) struct Context {
    /// Permissions (capabilities) at the moment of receiving a request.
    pub(crate) permissions: Option<Permissions>,
    /// Settings of the client API state machine.
    pub(crate) settings: ClientApiSettings,
    /// Matrix proxy that is used to send events and read events from the room.
    pub(crate) matrix: Box<dyn MatrixProxy>,
}

/// The result of the request handling.
pub(crate) struct HandleResult {
    /// Message that must be sent back to the widget (raw response).
    pub(crate) response: String,
    /// Post-action that must be performed by the caller (if any).
    /// Certain incoming requests are rather complicated and may
    /// require some additional actions to be performed by the caller
    /// after sending a response to the widget.
    pub(crate) action: Option<PostAction>,
}

impl<'de> Deserialize<'de> for IncomingMessage {
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Err(de::Error::custom("not implemented"))
    }
}
