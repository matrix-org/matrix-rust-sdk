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
    api::client::{account::request_openid_token, delayed_events},
    events::{AnyStateEvent, AnyTimelineEvent, AnyToDeviceEvent},
    serde::Raw,
};
use serde::{Deserialize, Deserializer, de};
use serde_json::value::RawValue as RawJsonValue;
use uuid::Uuid;

#[cfg(doc)]
use super::MatrixDriverRequestData;
use super::{
    SendToDeviceEventResponse,
    from_widget::{FromWidgetRequest, SendEventResponse},
    to_widget::ToWidgetResponse,
};
use crate::widget::Capabilities;

/// Incoming message for the widget client side module that it must process.
pub(crate) enum IncomingMessage {
    /// An incoming raw message from the widget.
    WidgetMessage(String),

    /// A response to a request to the `MatrixDriver`.
    MatrixDriverResponse {
        /// The ID of the request that this response corresponds to.
        request_id: Uuid,

        /// Result of the request: the response data, or a Matrix sdk error.
        ///
        /// Http errors will be forwarded to the widget in a specified format so
        /// the widget can parse the error.
        response: Result<MatrixDriverResponse, crate::Error>,
    },

    /// The `MatrixDriver` notified the `WidgetMachine` of a new Matrix event.
    ///
    /// This means that the machine previously subscribed to some events
    /// ([`crate::widget::Action::SubscribeTimeline`] request).
    MatrixEventReceived(Raw<AnyTimelineEvent>),

    /// The `MatrixDriver` notified the `WidgetMachine` of a change in room
    /// state.
    ///
    /// This means that the machine previously subscribed to some events
    /// ([`crate::widget::Action::Subscribe`] request).
    StateUpdateReceived(Vec<Raw<AnyStateEvent>>),

    /// The `MatrixDriver` notified the `WidgetMachine` of a new to-device
    /// event.
    ToDeviceReceived(Raw<AnyToDeviceEvent>),
}

pub(crate) enum MatrixDriverResponse {
    /// Client acquired capabilities from the user.
    /// A response to a [`MatrixDriverRequestData::AcquireCapabilities`]
    /// command.
    CapabilitiesAcquired(Capabilities),
    /// Client got OpenId token for a given request ID.
    /// A response to a [`MatrixDriverRequestData::GetOpenId`] command.
    OpenIdReceived(request_openid_token::v3::Response),
    /// Client read some Matrix event(s).
    /// A response to a [`MatrixDriverRequestData::ReadEvents`] command.
    EventsRead(Vec<Raw<AnyTimelineEvent>>),
    /// Client read some Matrix room state entries.
    /// A response to a [`MatrixDriverRequestData::ReadState`] command.
    StateRead(Vec<Raw<AnyStateEvent>>),
    /// Client sent some Matrix event. The response contains the event ID.
    /// A response to a [`MatrixDriverRequestData::SendEvent`] command.
    EventSent(SendEventResponse),
    /// A response to a `Action::SendToDevice` command.
    ToDeviceSent(SendToDeviceEventResponse),
    /// Client updated a delayed event.
    /// A response to a [`MatrixDriverRequestData::UpdateDelayedEvent`] command.
    DelayedEventUpdated(delayed_events::update_delayed_event::unstable::Response),
}

pub(super) struct IncomingWidgetMessage {
    pub(super) widget_id: String,
    pub(super) request_id: String,
    pub(super) kind: IncomingWidgetMessageKind,
}

#[derive(Debug)]
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
