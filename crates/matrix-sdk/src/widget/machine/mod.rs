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

use std::time::Duration;

use driver_req::UpdateDelayedEventRequest;
use from_widget::UpdateDelayedEventResponse;
use indexmap::IndexMap;
use ruma::{
    serde::{JsonObject, Raw},
    OwnedRoomId,
};
use serde::Serialize;
use serde_json::value::RawValue as RawJsonValue;
use tracing::{error, info, instrument, warn};
use uuid::Uuid;

use self::{
    driver_req::{
        AcquireCapabilities, MatrixDriverRequest, MatrixDriverRequestHandle,
        ReadMessageLikeEventRequest, RequestOpenId,
    },
    from_widget::{
        FromWidgetErrorResponse, FromWidgetRequest, ReadEventRequest, ReadEventResponse,
        SupportedApiVersionsResponse,
    },
    incoming::{IncomingWidgetMessage, IncomingWidgetMessageKind},
    openid::{OpenIdResponse, OpenIdState},
    pending::{PendingRequests, RequestLimits},
    to_widget::{
        NotifyCapabilitiesChanged, NotifyNewMatrixEvent, NotifyOpenIdChanged, RequestCapabilities,
        ToWidgetRequest, ToWidgetRequestHandle, ToWidgetResponse,
    },
};
#[cfg(doc)]
use super::WidgetDriver;
use super::{
    capabilities::{SEND_DELAYED_EVENT, UPDATE_DELAYED_EVENT},
    filter::FilterInput,
    Capabilities, StateKeySelector,
};
use crate::Result;

mod driver_req;
mod from_widget;
mod incoming;
mod openid;
mod pending;
#[cfg(test)]
mod tests;
mod to_widget;

pub(crate) use self::{
    driver_req::{MatrixDriverRequestData, ReadStateEventRequest, SendEventRequest},
    from_widget::SendEventResponse,
    incoming::{IncomingMessage, MatrixDriverResponse},
};

/// A command to perform in reaction to an [`IncomingMessage`].
///
/// There are also initial actions that may be performed at the creation of a
/// [`WidgetMachine`].
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
    Subscribe,

    /// Unsuscribe from the events in the *current* room. Symmetrical to
    /// `Subscribe`.
    Unsubscribe,
}

/// No I/O state machine.
///
/// Handles interactions with the widget as well as the
/// [`crate::widget::MatrixDriver`].
pub(crate) struct WidgetMachine {
    /// Unique identifier for the widget.
    ///
    /// Allows distinguishing different widgets.
    widget_id: String,

    /// The room to which this widget machine is attached.
    room_id: OwnedRoomId,

    /// Outstanding requests sent to the widget (mapped by uuid).
    pending_to_widget_requests: PendingRequests<ToWidgetRequestMeta>,

    /// Outstanding requests sent to the matrix driver (mapped by uuid).
    pending_matrix_driver_requests: PendingRequests<MatrixDriverRequestMeta>,

    /// Current negotiation state for capabilities.
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
        let limits =
            RequestLimits { max_pending_requests: 15, response_timeout: Duration::from_secs(10) };

        let mut machine = Self {
            widget_id,
            room_id,
            pending_to_widget_requests: PendingRequests::new(limits.clone()),
            pending_matrix_driver_requests: PendingRequests::new(limits),
            capabilities: CapabilitiesState::Unset,
        };

        let initial_actions =
            if init_on_content_load { Vec::new() } else { machine.negotiate_capabilities() };

        (machine, initial_actions)
    }

    /// Main entry point to drive the state machine.
    pub(crate) fn process(&mut self, event: IncomingMessage) -> Vec<Action> {
        // Clean up stale requests.
        self.pending_to_widget_requests.remove_expired();
        self.pending_matrix_driver_requests.remove_expired();

        match event {
            IncomingMessage::WidgetMessage(widget_message_raw) => {
                self.process_widget_message(&widget_message_raw)
            }
            IncomingMessage::MatrixDriverResponse { request_id, response } => {
                self.process_matrix_driver_response(request_id, response)
            }
            IncomingMessage::MatrixEventReceived(event) => {
                let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
                    error!("Received matrix event before capabilities negotiation");
                    return Vec::new();
                };

                capabilities
                    .allow_reading(&event)
                    .then(|| {
                        self.send_to_widget_request(NotifyNewMatrixEvent(event))
                            .map(|(_request, action)| vec![action])
                            .unwrap_or_default()
                    })
                    .unwrap_or_default()
            }
        }
    }

    fn process_widget_message(&mut self, raw: &str) -> Vec<Action> {
        let message = match serde_json::from_str::<IncomingWidgetMessage>(raw) {
            Ok(msg) => msg,
            Err(error) => {
                error!("couldn't deserialize incoming widget message: {error}");
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
            Err(e) => {
                return vec![Self::send_from_widget_err_response(
                    raw_request,
                    FromWidgetErrorResponse::from_error(crate::Error::SerdeJson(e)),
                )]
            }
        };

        match request {
            FromWidgetRequest::SupportedApiVersions {} => {
                let response = SupportedApiVersionsResponse::new();
                vec![Self::send_from_widget_response(raw_request, Ok(response))]
            }

            FromWidgetRequest::ContentLoaded {} => {
                let mut response =
                    vec![Self::send_from_widget_response(raw_request, Ok(JsonObject::new()))];
                if matches!(self.capabilities, CapabilitiesState::Unset) {
                    response.append(&mut self.negotiate_capabilities());
                }
                response
            }

            FromWidgetRequest::ReadEvent(req) => self
                .process_read_event_request(req, raw_request)
                .map(|a| vec![a])
                .unwrap_or_default(),

            FromWidgetRequest::SendEvent(req) => self
                .process_send_event_request(req, raw_request)
                .map(|a| vec![a])
                .unwrap_or_default(),

            FromWidgetRequest::GetOpenId {} => {
                let mut actions =
                    vec![Self::send_from_widget_response(raw_request, Ok(OpenIdResponse::Pending))];

                let Some((request, request_action)) =
                    self.send_matrix_driver_request(RequestOpenId)
                else {
                    // We're done, return early.
                    return actions;
                };

                request.then(|res, machine| {
                    let response = match res {
                        Ok(res) => OpenIdResponse::Allowed(OpenIdState::new(request_id, res)),
                        Err(msg) => {
                            info!("OpenID request failed: {msg}");
                            OpenIdResponse::Blocked { original_request_id: request_id }
                        }
                    };

                    machine
                        .send_to_widget_request(NotifyOpenIdChanged(response))
                        .map(|(_request, action)| vec![action])
                        .unwrap_or_default()
                });

                actions.push(request_action);
                actions
            }

            FromWidgetRequest::DelayedEventUpdate(req) => {
                let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
                    return vec![Self::send_from_widget_error_string_response(
                        raw_request,
                        "Received send update delayed event request before capabilities were negotiated"
                    )];
                };

                if !capabilities.update_delayed_event {
                    return vec![Self::send_from_widget_error_string_response(
                        raw_request,
                        format!("Not allowed: missing the {UPDATE_DELAYED_EVENT} capability."),
                    )];
                }

                self.send_matrix_driver_request(UpdateDelayedEventRequest {
                    action: req.action,
                    delay_id: req.delay_id,
                })
                .map(|(request, request_action)| {
                    request.then(|result, _machine| {
                        vec![Self::send_from_widget_response(
                            raw_request,
                            // This is mapped to another type because the
                            // update_delay_event::Response
                            // does not impl Serialize
                            result
                                .map(Into::<UpdateDelayedEventResponse>::into)
                                .map_err(FromWidgetErrorResponse::from_error),
                        )]
                    });

                    vec![request_action]
                })
                .unwrap_or_default()
            }
        }
    }

    fn process_read_event_request(
        &mut self,
        request: ReadEventRequest,
        raw_request: Raw<FromWidgetRequest>,
    ) -> Option<Action> {
        let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
            return Some(Self::send_from_widget_error_string_response(
                raw_request,
                "Received read event request before capabilities were negotiated",
            ));
        };

        match request {
            ReadEventRequest::ReadMessageLikeEvent { event_type, limit } => {
                if !capabilities.has_read_filter_for_type(&event_type) {
                    return Some(Self::send_from_widget_error_string_response(
                        raw_request,
                        "Not allowed to read message like event",
                    ));
                }

                const DEFAULT_EVENT_LIMIT: u32 = 50;
                let limit = limit.unwrap_or(DEFAULT_EVENT_LIMIT);
                let request = ReadMessageLikeEventRequest { event_type, limit };

                self.send_matrix_driver_request(request).map(|(request, action)| {
                    request.then(|result, machine| {
                        let response = match &machine.capabilities {
                            CapabilitiesState::Unset => Err(FromWidgetErrorResponse::from_string(
                                "Received read event request before capabilities negotiation",
                            )),
                            CapabilitiesState::Negotiating => {
                                Err(FromWidgetErrorResponse::from_string(
                                    "Received read event request while capabilities were negotiating",
                                ))
                            }
                            CapabilitiesState::Negotiated(capabilities) => result
                            .map(|mut events| {
                                events.retain(|e| capabilities.allow_reading(e));
                                ReadEventResponse { events }
                            })
                            .map_err(FromWidgetErrorResponse::from_error),
                        };

                        vec![Self::send_from_widget_response(raw_request, response)]
                    });

                    action
                })
            }

            ReadEventRequest::ReadStateEvent { event_type, state_key } => {
                let allowed = match state_key.clone() {
                    // If the widget tries to read any state event we can only skip sending the
                    // request, if the widget does not have any capability for
                    // the requested event type.
                    StateKeySelector::Any => capabilities.has_read_filter_for_type(&event_type),
                    // If we have a specific state key we will check if the widget has
                    // the capability to read this specific state key and otherwise
                    // skip sending the request.
                    StateKeySelector::Key(state_key) => {
                        capabilities.allow_reading(FilterInput::state(&event_type, &state_key))
                    }
                };
                if allowed {
                    self.send_matrix_driver_request(ReadStateEventRequest { event_type, state_key })
                        .map(|(request, action)| {
                            request.then(|result, _machine| {
                                let response = result
                                    .map(|events| ReadEventResponse { events })
                                    .map_err(FromWidgetErrorResponse::from_error);
                                vec![Self::send_from_widget_response(raw_request, response)]
                            });
                            action
                        })
                } else {
                    Some(Self::send_from_widget_error_string_response(
                        raw_request,
                        "Not allowed to read state event",
                    ))
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

        if !capabilities.send_delayed_event && request.delay.is_some() {
            return Some(Self::send_from_widget_error_string_response(
                raw_request,
                format!("Not allowed: missing the {SEND_DELAYED_EVENT} capability."),
            ));
        }

        if !capabilities.allow_sending(&request) {
            return Some(Self::send_from_widget_error_string_response(
                raw_request,
                "Not allowed to send event",
            ));
        }

        let (request, action) = self.send_matrix_driver_request(request)?;

        request.then(|mut result, machine| {
            if let Ok(r) = result.as_mut() {
                r.set_room_id(machine.room_id.clone());
            }
            vec![Self::send_from_widget_response(
                raw_request,
                result.map_err(FromWidgetErrorResponse::from_error),
            )]
        });

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

        let request = match self.pending_to_widget_requests.extract(&request_id) {
            Ok(r) => r,
            Err(e) => {
                warn!("{e}");
                return Vec::new();
            }
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
        response: Result<MatrixDriverResponse>,
    ) -> Vec<Action> {
        match self.pending_matrix_driver_requests.extract(&request_id) {
            Ok(request) => request
                .response_fn
                .map(|response_fn| response_fn(response, self))
                .unwrap_or_default(),
            Err(e) => {
                warn!("Could not find matching pending response: {e}");
                Vec::new()
            }
        }
    }

    fn send_from_widget_error_string_response(
        raw_request: Raw<FromWidgetRequest>,
        error: impl Into<String>,
    ) -> Action {
        Self::send_from_widget_err_response(
            raw_request,
            FromWidgetErrorResponse::from_string(error),
        )
    }

    fn send_from_widget_err_response(
        raw_request: Raw<FromWidgetRequest>,
        error: FromWidgetErrorResponse,
    ) -> Action {
        Self::send_from_widget_response(
            raw_request,
            Err::<serde_json::Value, FromWidgetErrorResponse>(error),
        )
    }

    fn send_from_widget_response(
        raw_request: Raw<FromWidgetRequest>,
        result: Result<impl Serialize, FromWidgetErrorResponse>,
    ) -> Action {
        // we do not want tho expose this to never allow sending arbitrary errors.
        // Errors always need to be `FromWidgetErrorResponse`.
        #[instrument(skip_all)]
        fn send_response_data(
            raw_request: Raw<FromWidgetRequest>,
            response_data: impl Serialize,
        ) -> Action {
            let f = || {
                let mut object =
                    raw_request.deserialize_as::<IndexMap<String, Box<RawJsonValue>>>()?;
                let response_data = serde_json::value::to_raw_value(&response_data)?;
                object.insert("response".to_owned(), response_data);
                serde_json::to_string(&object)
            };

            // SAFETY: we expect the raw request to be a valid JSON map, to which we add a
            // new field.
            let serialized = f().expect("error when attaching response to incoming request");

            Action::SendToWidget(serialized)
        }

        match result {
            Ok(res) => send_response_data(raw_request, res),
            Err(error_response) => send_response_data(raw_request, error_response),
        }
    }

    #[instrument(skip_all, fields(action = T::ACTION))]
    fn send_to_widget_request<T: ToWidgetRequest>(
        &mut self,
        to_widget_request: T,
    ) -> Option<(ToWidgetRequestHandle<'_, T::ResponseData>, Action)> {
        #[derive(Serialize)]
        #[serde(tag = "api", rename = "toWidget", rename_all = "camelCase")]
        struct ToWidgetRequestSerdeHelper<'a, T> {
            widget_id: &'a str,
            request_id: Uuid,
            action: &'static str,
            data: T,
        }

        let request_id = Uuid::new_v4();
        let full_request = ToWidgetRequestSerdeHelper {
            widget_id: &self.widget_id,
            request_id,
            action: T::ACTION,
            data: to_widget_request,
        };

        let request_meta = ToWidgetRequestMeta::new(T::ACTION);
        let Some(meta) = self.pending_to_widget_requests.insert(request_id, request_meta) else {
            warn!("Reached limits of pending requests for toWidget requests");
            return None;
        };

        let serialized = serde_json::to_string(&full_request).expect("Failed to serialize request");
        Some((ToWidgetRequestHandle::new(meta), Action::SendToWidget(serialized)))
    }

    #[instrument(skip_all)]
    fn send_matrix_driver_request<T: MatrixDriverRequest>(
        &mut self,
        request: T,
    ) -> Option<(MatrixDriverRequestHandle<'_, T::Response>, Action)> {
        let request_id = Uuid::new_v4();
        let request_meta = MatrixDriverRequestMeta::new();

        let Some(meta) = self.pending_matrix_driver_requests.insert(request_id, request_meta)
        else {
            warn!("Reached limits of pending requests for matrix driver requests");
            return None;
        };

        Some((
            MatrixDriverRequestHandle::new(meta),
            Action::MatrixDriverRequest { request_id, data: request.into() },
        ))
    }

    fn negotiate_capabilities(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();

        if matches!(&self.capabilities, CapabilitiesState::Negotiated(c) if !c.read.is_empty()) {
            actions.push(Action::Unsubscribe);
        }

        self.capabilities = CapabilitiesState::Negotiating;

        let Some((request, action)) = self.send_to_widget_request(RequestCapabilities {}) else {
            // We're done, return early.
            return actions;
        };

        request.then(|response, machine| {
            let requested_capabilities = response.capabilities;

            let Some((request, action)) = machine.send_matrix_driver_request(AcquireCapabilities {
                desired_capabilities: requested_capabilities.clone(),
            }) else {
                // We're done, return early.
                return Vec::new();
            };

            request.then(|result, machine| {
                let approved_capabilities = result.unwrap_or_else(|e| {
                    error!("Acquiring capabilities failed: {e}");
                    Capabilities::default()
                });

                let mut actions = Vec::new();
                if !approved_capabilities.read.is_empty() {
                    actions.push(Action::Subscribe);
                }

                machine.capabilities = CapabilitiesState::Negotiated(approved_capabilities.clone());

                let notify_caps_changed = NotifyCapabilitiesChanged {
                    approved: approved_capabilities,
                    requested: requested_capabilities,
                };

                if let Some(action) = machine
                    .send_to_widget_request(notify_caps_changed)
                    .map(|(_request, action)| action)
                {
                    actions.push(action);
                }

                actions
            });

            vec![action]
        });

        actions.push(action);
        actions
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
    Box<dyn FnOnce(Result<MatrixDriverResponse>, &mut WidgetMachine) -> Vec<Action> + Send>;

pub(crate) struct MatrixDriverRequestMeta {
    response_fn: Option<MatrixDriverResponseFn>,
}

impl MatrixDriverRequestMeta {
    fn new() -> Self {
        Self { response_fn: None }
    }
}

/// Current negotiation state for capabilities.
enum CapabilitiesState {
    /// Capabilities have never been defined.
    Unset,
    /// We're currently negotiating capabilities.
    Negotiating,
    /// The capabilities have already been negotiated.
    Negotiated(Capabilities),
}
