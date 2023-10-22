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

//! No I/O logic of the [`WidgetDriver`].

#![warn(unreachable_pub)]

use std::fmt;

use indexmap::{map::Entry, IndexMap};
use ruma::{
    serde::{JsonObject, Raw},
    OwnedRoomId,
};
use serde::Serialize;
use serde_json::value::RawValue as RawJsonValue;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

use self::{
    driver_req::{
        AcquireCapabilities, MatrixDriverRequest, MatrixDriverRequestHandle, RequestOpenId,
    },
    from_widget::{
        FromWidgetErrorResponse, FromWidgetRequest, ReadEventRequest, ReadEventResponse,
        SendEventResponse, SupportedApiVersionsResponse,
    },
    incoming::{IncomingWidgetMessage, IncomingWidgetMessageKind},
    openid::{OpenIdResponse, OpenIdState},
    to_widget::{
        NotifyCapabilitiesChanged, NotifyNewMatrixEvent, NotifyOpenIdChanged, RequestCapabilities,
        ToWidgetRequest, ToWidgetRequestHandle, ToWidgetResponse,
    },
};
#[cfg(doc)]
use super::WidgetDriver;
use super::{
    filter::{MatrixEventContent, MatrixEventFilterInput},
    Capabilities, StateKeySelector,
};

mod driver_req;
mod from_widget;
mod incoming;
mod openid;
#[cfg(test)]
mod tests;
mod to_widget;

pub(crate) use self::{
    driver_req::{MatrixDriverRequestData, ReadStateEventRequest, SendEventRequest},
    incoming::{IncomingMessage, MatrixDriverResponse},
};

/// Action (a command) that client (driver) must perform.
#[derive(Clone, Debug)]
pub(crate) enum Action {
    /// Send a raw message to the widget.
    SendToWidget(String),

    /// Command that is sent from the client widget API state machine to the
    /// client (driver) that must be performed. Once the command is executed,
    /// the client will typically generate an `Event` with the result of it.
    MatrixDriverRequest {
        /// Certain commands are typically answered with certain event once the
        /// command is performed. The api state machine will "tag" each command
        /// with some "cookie" (in this case just an ID), so that once the
        /// result of the execution of this command is received, it could be
        /// matched.
        request_id: Uuid,

        /// Data associated with this command.
        data: MatrixDriverRequestData,
    },

    /// Subscribe to the events in the *current* room, i.e. a room which this
    /// widget is instantiated with. The client is aware of the room.
    #[allow(dead_code)]
    Subscribe,

    /// Unsuscribe from the events in the *current* room. Symmetrical to
    /// `Subscribe`.
    #[allow(dead_code)]
    Unsubscribe,
}

/// No I/O state machine.
///
/// Handles interactions with the widget as well as the `MatrixDriver`.
pub(crate) struct WidgetMachine {
    widget_id: String,
    room_id: OwnedRoomId,
    pending_to_widget_requests: IndexMap<Uuid, ToWidgetRequestMeta>,
    pending_matrix_driver_requests: IndexMap<Uuid, MatrixDriverRequestMeta>,
    capabilities: CapabilitiesState,
}

impl WidgetMachine {
    /// Creates a new instance of a client widget API state machine.
    /// Returns the client api handler as well as the channel to receive
    /// actions (commands) from the client.
    pub(crate) fn new(
        widget_id: String,
        room_id: OwnedRoomId,
        init_on_content_load: bool,
    ) -> (Self, Vec<Action>) {
        let mut machine = Self {
            widget_id,
            room_id,
            pending_to_widget_requests: IndexMap::new(),
            pending_matrix_driver_requests: IndexMap::new(),
            capabilities: CapabilitiesState::Unset,
        };

        let actions = (!init_on_content_load).then(|| machine.negotiate_capabilities());
        (machine, actions.unwrap_or_default())
    }

    /// Main entry point to drive the state machine.
    pub(crate) fn process(&mut self, event: IncomingMessage) -> Vec<Action> {
        match event {
            IncomingMessage::WidgetMessage(raw) => self.process_widget_message(&raw),
            IncomingMessage::MatrixDriverResponse { request_id, response } => {
                self.process_matrix_driver_response(request_id, response)
            }
            IncomingMessage::MatrixEventReceived(event) => {
                let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
                    error!("Received matrix event before capabilities negotiation");
                    return Vec::new();
                };

                let filter_in = match event.deserialize_as::<MatrixEventFilterInput>() {
                    Ok(i) => i,
                    Err(e) => {
                        error!("Failed to deserialize event: {e}");
                        return Vec::new();
                    }
                };

                capabilities
                    .read
                    .iter()
                    .any(|f| f.matches(&filter_in))
                    .then(|| vec![self.send_to_widget_request(NotifyNewMatrixEvent(event)).1])
                    .unwrap_or_default()
            }
        }
    }

    fn process_widget_message(&mut self, raw: &str) -> Vec<Action> {
        let message = match serde_json::from_str::<IncomingWidgetMessage>(raw) {
            Ok(msg) => msg,
            Err(e) => {
                // TODO: There is a special error handling required for the invalid
                // messages. Refer to the `widget-api-poc` for implementation notes.
                error!("Failed to parse incoming message: {e}");
                return Vec::new();
            }
        };

        if message.widget_id != self.widget_id {
            error!("Received a message from a wrong widget, ignoring");
            return Vec::new();
        }

        match message.kind {
            IncomingWidgetMessageKind::Request(request) => {
                self.process_from_widget_request(message.request_id, request)
            }
            IncomingWidgetMessageKind::Response(response) => {
                self.process_to_widget_response(message.request_id, response)
            }
        }
    }

    #[instrument(skip_all, fields(?request_id))]
    fn process_from_widget_request(
        &mut self,
        request_id: String,
        raw_request: Raw<FromWidgetRequest>,
    ) -> Vec<Action> {
        let request = match raw_request.deserialize() {
            Ok(r) => r,
            Err(e) => return vec![self.send_from_widget_error_response(raw_request, e)],
        };

        match request {
            FromWidgetRequest::SupportedApiVersions {} => {
                let response = SupportedApiVersionsResponse::new();
                vec![self.send_from_widget_response(raw_request, response)]
            }

            FromWidgetRequest::ContentLoaded {} => {
                let response = vec![self.send_from_widget_response(raw_request, JsonObject::new())];
                self.capabilities
                    .is_unset()
                    .then(|| [&response, self.negotiate_capabilities().as_slice()].concat())
                    .unwrap_or(response)
            }

            FromWidgetRequest::ReadEvent(req) => {
                vec![self.process_read_event_request(req, raw_request)]
            }

            FromWidgetRequest::SendEvent(req) => self
                .process_send_event_request(req, raw_request)
                .map(|a| vec![a])
                .unwrap_or_default(),

            FromWidgetRequest::GetOpenId {} => {
                let (request, request_action) = self.send_matrix_driver_request(RequestOpenId);
                request.then(|res, machine| {
                    let response = match res {
                        Ok(res) => OpenIdResponse::Allowed(OpenIdState::new(request_id, res)),
                        Err(msg) => {
                            info!("OpenID request failed: {msg}");
                            OpenIdResponse::Blocked { original_request_id: request_id }
                        }
                    };

                    vec![machine.send_to_widget_request(NotifyOpenIdChanged(response)).1]
                });

                let response = self.send_from_widget_response(raw_request, OpenIdResponse::Pending);
                vec![response, request_action]
            }
        }
    }

    fn process_read_event_request(
        &mut self,
        request: ReadEventRequest,
        raw_request: Raw<FromWidgetRequest>,
    ) -> Action {
        let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
            let text = "Received read event request before capabilities were negotiated";
            return self.send_from_widget_error_response(raw_request, text);
        };

        match request {
            ReadEventRequest::ReadMessageLikeEvent { .. } => {
                let text = "Reading of message events is not yet supported";
                self.send_from_widget_error_response(raw_request, text)
            }
            ReadEventRequest::ReadStateEvent { event_type, state_key } => {
                let allowed = match &state_key {
                    StateKeySelector::Any => capabilities
                        .read
                        .iter()
                        .any(|filter| filter.matches_state_event_with_any_state_key(&event_type)),

                    StateKeySelector::Key(state_key) => {
                        let filter_in = MatrixEventFilterInput {
                            event_type: event_type.to_string().into(),
                            state_key: Some(state_key.clone()),
                            // content doesn't matter for state events
                            content: MatrixEventContent::default(),
                        };

                        capabilities.read.iter().any(|filter| filter.matches(&filter_in))
                    }
                };

                if allowed {
                    let request = ReadStateEventRequest { event_type, state_key };
                    let (request, action) = self.send_matrix_driver_request(request);
                    request.then(|result, machine| {
                        let response = result.map(|events| ReadEventResponse { events });
                        vec![machine.send_from_widget_result_response(raw_request, response)]
                    });
                    action
                } else {
                    self.send_from_widget_error_response(raw_request, "Not allowed")
                }
            }
        }
    }

    fn process_send_event_request(
        &mut self,
        request: SendEventRequest,
        raw_request: Raw<FromWidgetRequest>,
    ) -> Option<Action> {
        let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
            error!("Received send event request before capabilities negotiation");
            return None;
        };

        let filter_in = MatrixEventFilterInput {
            event_type: request.event_type.clone(),
            state_key: request.state_key.clone(),
            content: serde_json::from_value(request.content.clone()).unwrap_or_else(|e| {
                debug!("Failed to deserialize event content for filter: {e}");
                // Fallback to empty content is safe because there is no filter
                // that matches with it when it otherwise wouldn't.
                Default::default()
            }),
        };

        let action = if capabilities.send.iter().any(|filter| filter.matches(&filter_in)) {
            let (request, action) = self.send_matrix_driver_request(request);
            request.then(|result, machine| {
                let room_id = &machine.room_id;
                let response = result.map(|event_id| SendEventResponse { event_id, room_id });
                vec![machine.send_from_widget_result_response(raw_request, response)]
            });
            action
        } else {
            self.send_from_widget_error_response(raw_request, "Not allowed")
        };

        Some(action)
    }

    #[instrument(skip_all, fields(?request_id))]
    fn process_to_widget_response(
        &mut self,
        request_id: String,
        response: ToWidgetResponse,
    ) -> Vec<Action> {
        let Ok(request_id) = Uuid::parse_str(&request_id) else {
            error!("Response's request_id is not a valid UUID");
            return Vec::new();
        };

        let Some(request) = self.pending_to_widget_requests.remove(&request_id) else {
            warn!("Received response for an unknown request");
            return Vec::new();
        };

        if response.action != request.action {
            error!(
                ?request.action, ?response.action,
                "Received response with different `action` than request"
            );
            return Vec::new();
        }

        request
            .response_fn
            .map(|response_fn| response_fn(response.response_data, self))
            .unwrap_or_default()
    }

    #[instrument(skip_all, fields(?request_id))]
    fn process_matrix_driver_response(
        &mut self,
        request_id: Uuid,
        response: Result<MatrixDriverResponse, String>,
    ) -> Vec<Action> {
        let Some(request) = self.pending_matrix_driver_requests.remove(&request_id) else {
            error!("Received response for an unknown request");
            return Vec::new();
        };

        request.response_fn.map(|response_fn| response_fn(response, self)).unwrap_or_default()
    }

    #[instrument(skip_all, fields(request_id))]
    fn send_from_widget_response(
        &self,
        raw_request: Raw<FromWidgetRequest>,
        response_data: impl Serialize,
    ) -> Action {
        let mut object = raw_request
            .deserialize_as::<IndexMap<String, Box<RawJsonValue>>>()
            .expect("Failed to converted FromWidgetRequest to object representation");
        let response_data = serde_json::value::to_raw_value(&response_data)
            .expect("Failed to serialize response data");
        object.insert("response".to_owned(), response_data);
        let serialized = serde_json::to_string(&object).expect("Failed to serialize response");
        Action::SendToWidget(serialized)
    }

    fn send_from_widget_error_response(
        &self,
        raw_request: Raw<FromWidgetRequest>,
        error: impl fmt::Display,
    ) -> Action {
        self.send_from_widget_response(raw_request, FromWidgetErrorResponse::new(error))
    }

    fn send_from_widget_result_response(
        &self,
        raw_request: Raw<FromWidgetRequest>,
        result: Result<impl Serialize, impl fmt::Display>,
    ) -> Action {
        match result {
            Ok(res) => self.send_from_widget_response(raw_request, res),
            Err(msg) => self.send_from_widget_error_response(raw_request, msg),
        }
    }

    #[instrument(skip_all, fields(action = T::ACTION))]
    fn send_to_widget_request<T: ToWidgetRequest>(
        &mut self,
        to_widget_request: T,
    ) -> (ToWidgetRequestHandle<'_, T::ResponseData>, Action) {
        #[derive(Serialize)]
        #[serde(tag = "api", rename = "toWidget", rename_all = "camelCase")]
        struct ToWidgetRequestSerHelper<'a, T> {
            widget_id: &'a str,
            request_id: Uuid,
            action: &'static str,
            data: T,
        }

        let request_id = Uuid::new_v4();
        let full_request = ToWidgetRequestSerHelper {
            widget_id: &self.widget_id,
            request_id,
            action: T::ACTION,
            data: to_widget_request,
        };

        let request_meta = ToWidgetRequestMeta::new(T::ACTION);
        let Entry::Vacant(entry) = self.pending_to_widget_requests.entry(request_id) else {
            panic!("uuid collision");
        };
        let meta = entry.insert(request_meta);

        let serialized = serde_json::to_string(&full_request).expect("Failed to serialize request");
        (ToWidgetRequestHandle::new(meta), Action::SendToWidget(serialized))
    }

    #[instrument(skip_all)]
    fn send_matrix_driver_request<T: MatrixDriverRequest>(
        &mut self,
        request: T,
    ) -> (MatrixDriverRequestHandle<'_, T::Response>, Action) {
        let request_id = Uuid::new_v4();
        let Entry::Vacant(entry) = self.pending_matrix_driver_requests.entry(request_id) else {
            panic!("uuid collision");
        };
        let meta = entry.insert(MatrixDriverRequestMeta::new());
        let action = Action::MatrixDriverRequest { request_id, data: request.into() };
        (MatrixDriverRequestHandle::new(meta), action)
    }

    fn negotiate_capabilities(&mut self) -> Vec<Action> {
        let unsubscribe_required =
            matches!(&self.capabilities, CapabilitiesState::Negotiated(c) if !c.read.is_empty());
        self.capabilities = CapabilitiesState::Negotiating;

        let (request, action) = self.send_to_widget_request(RequestCapabilities {});
        request.then(|response, machine| {
            let requested = response.capabilities;
            let (request, action) = machine.send_matrix_driver_request(AcquireCapabilities {
                desired_capabilities: requested.clone(),
            });

            request.then(|result, machine| {
                let approved = result.unwrap_or_else(|e| {
                    error!("Acquiring capabilities failed: {e}");
                    Capabilities::default()
                });

                let subscribe_required = !approved.read.is_empty();
                machine.capabilities = CapabilitiesState::Negotiated(approved.clone());

                let update = NotifyCapabilitiesChanged { approved, requested };
                let (_request, action) = machine.send_to_widget_request(update);

                (subscribe_required)
                    .then(|| Action::Subscribe)
                    .into_iter()
                    .chain(Some(action))
                    .collect()
            });

            vec![action]
        });

        unsubscribe_required.then(|| Action::Unsubscribe).into_iter().chain(Some(action)).collect()
    }
}

type ToWidgetResponseFn =
    Box<dyn FnOnce(Box<RawJsonValue>, &mut WidgetMachine) -> Vec<Action> + Send>;

pub(crate) struct ToWidgetRequestMeta {
    action: &'static str,
    response_fn: Option<ToWidgetResponseFn>,
}

impl ToWidgetRequestMeta {
    fn new(action: &'static str) -> Self {
        Self { action, response_fn: None }
    }
}

type MatrixDriverResponseFn =
    Box<dyn FnOnce(Result<MatrixDriverResponse, String>, &mut WidgetMachine) -> Vec<Action> + Send>;

pub(crate) struct MatrixDriverRequestMeta {
    response_fn: Option<MatrixDriverResponseFn>,
}

impl MatrixDriverRequestMeta {
    fn new() -> Self {
        Self { response_fn: None }
    }
}

enum CapabilitiesState {
    Unset,
    Negotiating,
    Negotiated(Capabilities),
}

impl CapabilitiesState {
    #[must_use]
    fn is_unset(&self) -> bool {
        matches!(self, Self::Unset)
    }
}
