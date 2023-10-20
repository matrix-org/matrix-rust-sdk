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
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{debug, error, info, instrument, trace, warn};
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
        NotifyNewMatrixEvent, NotifyOpenIdChanged, NotifyPermissionsChanged, RequestCapabilities,
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
#[derive(Debug)]
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
    actions_sender: UnboundedSender<Action>,
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
    ) -> (Self, UnboundedReceiver<Action>) {
        let (actions_sender, actions_receiver) = unbounded_channel();
        let mut machine = Self {
            widget_id,
            room_id,
            actions_sender,
            pending_to_widget_requests: IndexMap::new(),
            pending_matrix_driver_requests: IndexMap::new(),
            capabilities: CapabilitiesState::Unset,
        };

        if !init_on_content_load {
            machine.negotiate_capabilities();
        }

        (machine, actions_receiver)
    }

    /// Main entry point to drive the state machine.
    pub(crate) fn process(&mut self, event: IncomingMessage) {
        match event {
            IncomingMessage::WidgetMessage(raw) => {
                self.process_widget_message(&raw);
            }
            IncomingMessage::MatrixDriverResponse { request_id, response } => {
                self.process_matrix_driver_response(request_id, response);
            }
            IncomingMessage::MatrixEventReceived(event) => {
                let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
                    error!("Received matrix event before capabilities negotiation");
                    return;
                };

                let filter_in = match event.deserialize_as::<MatrixEventFilterInput>() {
                    Ok(i) => i,
                    Err(e) => {
                        error!("Failed to deserialize event: {e}");
                        return;
                    }
                };

                if capabilities.read.iter().any(|f| f.matches(&filter_in)) {
                    self.send_to_widget_request(NotifyNewMatrixEvent(event));
                }
            }
        }
    }

    fn process_widget_message(&mut self, raw: &str) {
        let message = match serde_json::from_str::<IncomingWidgetMessage>(raw) {
            Ok(msg) => msg,
            Err(e) => {
                // TODO: There is a special error handling required for the invalid
                // messages. Refer to the `widget-api-poc` for implementation notes.
                error!("Failed to parse incoming message: {e}");
                return;
            }
        };

        if message.widget_id != self.widget_id {
            error!("Received a message from a wrong widget, ignoring");
            return;
        }

        match message.kind {
            IncomingWidgetMessageKind::Request(request) => {
                self.process_from_widget_request(message.request_id, request);
            }
            IncomingWidgetMessageKind::Response(response) => {
                self.process_to_widget_response(message.request_id, response);
            }
        }
    }

    #[instrument(skip_all, fields(?request_id))]
    fn process_from_widget_request(
        &mut self,
        request_id: String,
        raw_request: Raw<FromWidgetRequest>,
    ) {
        let request = match raw_request.deserialize() {
            Ok(r) => r,
            Err(e) => {
                self.send_from_widget_error_response(raw_request, e);
                return;
            }
        };

        match request {
            FromWidgetRequest::SupportedApiVersions {} => {
                self.send_from_widget_response(raw_request, SupportedApiVersionsResponse::new());
            }

            FromWidgetRequest::ContentLoaded {} => {
                self.send_from_widget_response(raw_request, JsonObject::new());
                if self.capabilities.is_unset() {
                    self.negotiate_capabilities();
                }
            }

            FromWidgetRequest::ReadEvent(req) => {
                self.process_read_event_request(req, raw_request);
            }

            FromWidgetRequest::SendEvent(req) => {
                self.process_send_event_request(req, raw_request);
            }

            FromWidgetRequest::GetOpenId {} => {
                self.send_from_widget_response(raw_request, OpenIdResponse::Pending);
                self.send_matrix_driver_request(RequestOpenId).then(|res, machine| {
                    let response = match res {
                        Ok(res) => OpenIdResponse::Allowed(OpenIdState::new(request_id, res)),
                        Err(msg) => {
                            info!("OpenID request failed: {msg}");
                            OpenIdResponse::Blocked { original_request_id: request_id }
                        }
                    };

                    machine.send_to_widget_request(NotifyOpenIdChanged(response));
                });
            }
        }
    }

    fn process_read_event_request(
        &mut self,
        request: ReadEventRequest,
        raw_request: Raw<FromWidgetRequest>,
    ) {
        let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
            self.send_from_widget_error_response(
                raw_request,
                "Received read event request before capabilities were negotiated",
            );
            return;
        };

        match request {
            ReadEventRequest::ReadMessageLikeEvent { .. } => {
                self.send_from_widget_error_response(
                    raw_request,
                    "Reading of message events is not yet supported",
                );
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
                    self.send_matrix_driver_request(request).then(|result, machine| {
                        let response = result.map(|events| ReadEventResponse { events });
                        machine.send_from_widget_result_response(raw_request, response);
                    });
                } else {
                    self.send_from_widget_error_response(raw_request, "Not allowed");
                }
            }
        }
    }

    fn process_send_event_request(
        &mut self,
        request: SendEventRequest,
        raw_request: Raw<FromWidgetRequest>,
    ) {
        let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
            error!("Received send event request before capabilities negotiation");
            return;
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

        if capabilities.send.iter().any(|filter| filter.matches(&filter_in)) {
            self.send_matrix_driver_request(request).then(|result, machine| {
                let response = result
                    .map(|event_id| SendEventResponse { event_id, room_id: &machine.room_id });
                machine.send_from_widget_result_response(raw_request, response);
            });
        } else {
            self.send_from_widget_error_response(raw_request, "Not allowed");
        }
    }

    #[instrument(skip_all, fields(?request_id))]
    fn process_to_widget_response(&mut self, request_id: String, response: ToWidgetResponse) {
        let Ok(request_id) = Uuid::parse_str(&request_id) else {
            error!("Response's request_id is not a valid UUID");
            return;
        };

        let Some(request) = self.pending_to_widget_requests.remove(&request_id) else {
            warn!("Received response for an unknown request");
            return;
        };

        if response.action != request.action {
            error!(
                ?request.action, ?response.action,
                "Received response with different `action` than request"
            );
        }

        if let Some(response_fn) = request.response_fn {
            trace!("Calling response_fn");
            response_fn(response.response_data, self);
        } else {
            trace!("No response_fn registered");
        }
    }

    #[instrument(skip_all, fields(?request_id))]
    fn process_matrix_driver_response(
        &mut self,
        request_id: Uuid,
        response: Result<MatrixDriverResponse, String>,
    ) {
        let Some(request) = self.pending_matrix_driver_requests.remove(&request_id) else {
            error!("Received response for an unknown request");
            return;
        };

        if let Some(response_fn) = request.response_fn {
            trace!("Calling response_fn");
            response_fn(response, self);
        } else {
            trace!("No response_fn registered");
        }
    }

    #[instrument(skip_all, fields(request_id))]
    fn send_from_widget_response(
        &self,
        raw_request: Raw<FromWidgetRequest>,
        response_data: impl Serialize,
    ) {
        let mut object = match raw_request.deserialize_as::<IndexMap<String, Box<RawJsonValue>>>() {
            Ok(o) => o,
            Err(e) => {
                error!("Failed to converted FromWidgetRequest to object representation: {e}");
                return;
            }
        };
        let response_data = match serde_json::value::to_raw_value(&response_data) {
            Ok(d) => d,
            Err(e) => {
                error!("Failed to serialize response data: {e}");
                return;
            }
        };
        object.insert("response".to_owned(), response_data);

        let serialized = match serde_json::to_string(&object) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to serialize response: {e}");
                return;
            }
        };

        if let Err(e) = self.actions_sender.send(Action::SendToWidget(serialized)) {
            error!("Failed to send action: {e}");
        }
    }

    fn send_from_widget_error_response(
        &self,
        raw_request: Raw<FromWidgetRequest>,
        error: impl fmt::Display,
    ) {
        self.send_from_widget_response(raw_request, FromWidgetErrorResponse::new(error))
    }

    fn send_from_widget_result_response(
        &self,
        raw_request: Raw<FromWidgetRequest>,
        result: Result<impl Serialize, impl fmt::Display>,
    ) {
        match result {
            Ok(res) => self.send_from_widget_response(raw_request, res),
            Err(msg) => self.send_from_widget_error_response(raw_request, msg),
        }
    }

    #[instrument(skip_all, fields(action = T::ACTION))]
    fn send_to_widget_request<T: ToWidgetRequest>(
        &mut self,
        to_widget_request: T,
    ) -> ToWidgetRequestHandle<'_, T::ResponseData> {
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

        let serialized = match serde_json::to_string(&full_request) {
            Ok(msg) => msg,
            Err(e) => {
                error!("Failed to serialize outgoing message: {e}");
                return ToWidgetRequestHandle::null();
            }
        };

        if let Err(e) = self.actions_sender.send(Action::SendToWidget(serialized)) {
            error!("Failed to send action: {e}");
            return ToWidgetRequestHandle::null();
        }

        let request_meta = ToWidgetRequestMeta::new(T::ACTION);
        let Entry::Vacant(entry) = self.pending_to_widget_requests.entry(request_id) else {
            panic!("uuid collision");
        };
        let meta = entry.insert(request_meta);

        ToWidgetRequestHandle::new(meta)
    }

    #[instrument(skip_all)]
    fn send_matrix_driver_request<T: MatrixDriverRequest>(
        &mut self,
        matrix_driver_request: T,
    ) -> MatrixDriverRequestHandle<'_, T::Response> {
        let request_id = Uuid::new_v4();
        if let Err(e) = self
            .actions_sender
            .send(Action::MatrixDriverRequest { request_id, data: matrix_driver_request.into() })
        {
            error!("Failed to send action: {e}");
            return MatrixDriverRequestHandle::null();
        }

        let request_meta = MatrixDriverRequestMeta::new();
        let Entry::Vacant(entry) = self.pending_matrix_driver_requests.entry(request_id) else {
            panic!("uuid collision");
        };
        let meta = entry.insert(request_meta);

        MatrixDriverRequestHandle::new(meta)
    }

    fn negotiate_capabilities(&mut self) {
        if let CapabilitiesState::Negotiated(capabilities) = &self.capabilities {
            if !capabilities.read.is_empty() {
                if let Err(err) = self.actions_sender.send(Action::Unsubscribe) {
                    error!("Failed to send action: {err}");
                }
            }
        }

        self.capabilities = CapabilitiesState::Negotiating;

        self.send_to_widget_request(RequestCapabilities {})
            // TODO: Each request can actually fail here, take this into an account.
            .then(|response, machine| {
                let requested = response.capabilities;
                machine
                    .send_matrix_driver_request(AcquireCapabilities {
                        desired_capabilities: requested.clone(),
                    })
                    .then(|result, machine| {
                        let approved = result.unwrap_or_else(|e| {
                            error!("Acquiring capabilities failed: {e}");
                            Capabilities::default()
                        });

                        if !approved.read.is_empty() {
                            if let Err(err) = machine.actions_sender.send(Action::Subscribe) {
                                error!("Failed to send action: {err}");
                            }
                        }

                        machine.capabilities = CapabilitiesState::Negotiated(approved.clone());
                        machine.send_to_widget_request(NotifyPermissionsChanged {
                            approved,
                            requested,
                        });
                    })
            });
    }
}

type ToWidgetResponseFn = Box<dyn FnOnce(Box<RawJsonValue>, &mut WidgetMachine) + Send>;

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
    Box<dyn FnOnce(Result<MatrixDriverResponse, String>, &mut WidgetMachine) + Send>;

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
