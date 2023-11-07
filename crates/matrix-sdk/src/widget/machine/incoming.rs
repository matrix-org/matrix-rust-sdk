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

use ruma::{
    api::client::account::request_openid_token, events::AnyTimelineEvent, serde::Raw, OwnedEventId,
};
use serde::{de, Deserialize, Deserializer};
use serde_json::value::RawValue as RawJsonValue;
use uuid::Uuid;

use super::{from_widget::FromWidgetRequest, to_widget::ToWidgetResponse};
use crate::widget::Capabilities;

/// Incoming event that the client API must process.
pub(crate) enum IncomingMessage {
    /// An incoming raw message from the widget.
    WidgetMessage(String),

    /// A response to a request to the `MatrixDriver`.
    MatrixDriverResponse {
        /// The ID of the request that this response corresponds to.
        request_id: Uuid,

        /// The result of the request: response data or error message.
        response: Result<MatrixDriverResponse, String>,
    },

    /// The `MatrixDriver` notified the `WidgetMachine` of a new matrix event.
    ///
    /// This means that the machine previously subscribed to some events
    /// (`Action::Subscribe` request).
    MatrixEventReceived(Raw<AnyTimelineEvent>),
}

pub(crate) enum MatrixDriverResponse {
    /// Client acquired capabilities from the user.
    ///
    /// A response to an `Action::AcquireCapabilities` command.
    CapabilitiesAcquired(Capabilities),
    /// Client got OpenId token for a given request ID.
    /// A response to an `Action::GetOpenId` command.
    OpenIdReceived(request_openid_token::v3::Response),
    /// Client read some matrix event(s).
    /// A response to an `Action::ReadMatrixEvent` commands.
    MatrixEventRead(Vec<Raw<AnyTimelineEvent>>),
    /// Client sent some matrix event. The response contains the event ID.
    /// A response to an `Action::SendMatrixEvent` command.
    MatrixEventSent(OwnedEventId),
}

pub(super) struct IncomingWidgetMessage {
    pub(super) widget_id: String,
    pub(super) request_id: String,
    pub(super) kind: IncomingWidgetMessageKind,
}

pub(super) enum IncomingWidgetMessageKind {
    Request(Raw<FromWidgetRequest>),
    Response(ToWidgetResponse),
}

impl<'de> Deserialize<'de> for IncomingWidgetMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw: Box<RawJsonValue> = Box::deserialize(deserializer)?;

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        enum ApiTag {
            FromWidget,
            ToWidget,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ExtractHeader {
            api: ApiTag,
            widget_id: String,
            request_id: String,
        }

        let ExtractHeader { api, widget_id, request_id } =
            serde_json::from_str(raw.get()).map_err(de::Error::custom)?;

        let kind = match api {
            ApiTag::FromWidget => IncomingWidgetMessageKind::Request(Raw::from_json(raw)),
            ApiTag::ToWidget => serde_json::from_str(raw.get())
                .map(IncomingWidgetMessageKind::Response)
                .map_err(de::Error::custom)?,
        };

        Ok(Self { widget_id, request_id, kind })
    }
}
