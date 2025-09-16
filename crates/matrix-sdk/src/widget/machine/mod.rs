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

use driver_req::{ReadStateRequest, UpdateDelayedEventRequest};
use from_widget::UpdateDelayedEventResponse;
use indexmap::IndexMap;
use ruma::{
    OwnedRoomId,
    events::{AnyStateEvent, AnyTimelineEvent},
    serde::{JsonObject, Raw},
};
use serde::Serialize;
use serde_json::value::RawValue as RawJsonValue;
use to_widget::NotifyNewToDeviceMessage;
use tracing::{error, info, instrument, warn};
use uuid::Uuid;

use self::{
    driver_req::{
        AcquireCapabilities, MatrixDriverRequest, MatrixDriverRequestHandle, RequestOpenId,
    },
    from_widget::{
        FromWidgetErrorResponse, FromWidgetRequest, ReadEventsResponse,
        SupportedApiVersionsResponse,
    },
    incoming::{IncomingWidgetMessage, IncomingWidgetMessageKind},
    openid::{OpenIdResponse, OpenIdState},
    pending::{PendingRequests, RequestLimits},
    to_widget::{
        NotifyCapabilitiesChanged, NotifyNewMatrixEvent, NotifyOpenIdChanged, NotifyStateUpdate,
        RequestCapabilities, ToWidgetRequest, ToWidgetRequestHandle, ToWidgetResponse,
    },
};
#[cfg(doc)]
use super::WidgetDriver;
use super::{
    Capabilities, StateEventFilter, StateKeySelector,
    capabilities::{SEND_DELAYED_EVENT, UPDATE_DELAYED_EVENT},
    filter::FilterInput,
};
use crate::{Error, Result, widget::Filter};

mod driver_req;
mod from_widget;
mod incoming;
mod openid;
mod pending;
#[cfg(test)]
mod tests;
mod to_widget;

pub(crate) use self::{
    driver_req::{MatrixDriverRequestData, SendEventRequest, SendToDeviceRequest},
    from_widget::{SendEventResponse, SendToDeviceEventResponse},
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

    /// Subscribe to the events that the widget capabilities allow,
    /// in the _current_ room, i.e. a room which this widget is instantiated
    /// with. The client is aware of the room.
    Subscribe,

    /// Unsubscribe from the events that the widget capabilities allow,
    /// in the _current_ room. Symmetrical to `Subscribe`.
    Unsubscribe,
}

/// An initial state update which is in the process of being computed.
#[derive(Debug)]
struct InitialStateUpdate {
    /// The results of the read state requests which establish the initial state
    /// to be pushed to the widget.
    initial_state: Vec<Vec<Raw<AnyStateEvent>>>,
    /// The total number of read state requests that `initial_state` must hold
    /// for this update to be considered complete.
    request_count: usize,
    /// The data carried by any state updates which raced with the requests to
    /// read the initial state. These should be pushed to the widget
    /// immediately after pushing the initial state to ensure no data is
    /// lost.
    postponed_updates: Vec<Vec<Raw<AnyStateEvent>>>,
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

    /// Outstanding requests sent to the Matrix driver (mapped by uuid).
    pending_matrix_driver_requests: PendingRequests<MatrixDriverRequestMeta>,

    /// Outstanding state updates waiting to be sent to the widget.
    ///
    /// Whenever the widget is approved to read a set of room state entries, we
    /// want to push an initial state to the widget in a single
    /// [`NotifyStateUpdate`] action. However, multiple asynchronous
    /// requests must be sent to the driver to gather this data. Therefore
    /// we use this field to hold the responses to the driver requests while
    /// some of them are still in flight. It is set to `Some` whenever the
    /// widget is approved to read some room state, and reset to `None` as
    /// soon as the [`NotifyStateUpdate`] action is emitted.
    pending_state_updates: Option<InitialStateUpdate>,

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
            pending_state_updates: None,
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
            IncomingMessage::MatrixEventReceived(event_raw) => {
                let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
                    error!("Received Matrix event before capabilities negotiation");
                    return Vec::new();
                };

                if capabilities.allow_reading(&event_raw) {
                    self.send_to_widget_request(NotifyNewMatrixEvent(event_raw))
                        .map(|(_request, action)| vec![action])
                        .unwrap_or_default()
                } else {
                    vec![]
                }
            }
            IncomingMessage::ToDeviceReceived(to_device_raw) => {
                let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
                    error!("Received to device event before capabilities negotiation");
                    return Vec::new();
                };

                if capabilities.allow_reading(&to_device_raw) {
                    self.send_to_widget_request(NotifyNewToDeviceMessage(to_device_raw))
                        .map(|(_request, action)| vec![action])
                        .unwrap_or_default()
                } else {
                    vec![]
                }
            }
            IncomingMessage::StateUpdateReceived(mut state) => {
                let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
                    error!("Received state update before capabilities negotiation");
                    return Vec::new();
                };

                state.retain(|event| capabilities.allow_reading(event));

                match &mut self.pending_state_updates {
                    Some(InitialStateUpdate { postponed_updates, .. }) => {
                        // This state update is racing with the read requests used to calculate the
                        // initial state; postpone it
                        postponed_updates.push(state);
                        Vec::new()
                    }
                    None => self.send_state_update(state).into_iter().collect(),
                }
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
                    FromWidgetErrorResponse::from_error(Error::SerdeJson(e)),
                )];
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

            FromWidgetRequest::SendToDevice(req) => self
                .process_to_device_request(req, raw_request)
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

                request.add_response_handler(|res, machine| {
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
                        "Received send update delayed event request \
                         before capabilities were negotiated",
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
                    request.add_response_handler(|result, _machine| {
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

    /// Send a response to a request to read events.
    ///
    /// `events` represents the message-like events provided by the
    /// [`crate::widget::MatrixDriver`].
    fn send_read_events_response(
        &self,
        request: Raw<FromWidgetRequest>,
        events: Result<Vec<Raw<AnyTimelineEvent>>, Error>,
    ) -> Vec<Action> {
        let response = match &self.capabilities {
            CapabilitiesState::Unset => Err(FromWidgetErrorResponse::from_string(
                "Received read events request before capabilities negotiation",
            )),
            CapabilitiesState::Negotiating => Err(FromWidgetErrorResponse::from_string(
                "Received read events request while capabilities were negotiating",
            )),
            CapabilitiesState::Negotiated(capabilities) => events
                .map(|mut events| {
                    events.retain(|e| capabilities.allow_reading(e));
                    ReadEventsResponse { events }
                })
                .map_err(FromWidgetErrorResponse::from_error),
        };

        vec![WidgetMachine::send_from_widget_response(request, response)]
    }

    fn process_read_event_request(
        &mut self,
        request: from_widget::ReadEventsRequest,
        raw_request: Raw<FromWidgetRequest>,
    ) -> Option<Action> {
        let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
            return Some(Self::send_from_widget_error_string_response(
                raw_request,
                "Received read event request before capabilities were negotiated",
            ));
        };

        // Check the event type and state key filter against the capabilities
        match &request.state_key {
            None => {
                if !capabilities.has_read_filter_for_type(&request.event_type) {
                    return Some(Self::send_from_widget_error_string_response(
                        raw_request,
                        "Not allowed to read message-like event",
                    ));
                }
            }
            Some(state_key) => {
                let allowed = match state_key.clone() {
                    // If the widget tries to read any state event we can only skip sending the
                    // request, if the widget does not have any capability for
                    // the requested event type.
                    StateKeySelector::Any => {
                        capabilities.has_read_filter_for_type(&request.event_type)
                    }
                    // If we have a specific state key we will check if the widget has
                    // the capability to read this specific state key and otherwise
                    // skip sending the request.
                    StateKeySelector::Key(state_key) => capabilities
                        .allow_reading(FilterInput::state(&request.event_type, &state_key)),
                };

                if !allowed {
                    return Some(Self::send_from_widget_error_string_response(
                        raw_request,
                        "Not allowed to read state event",
                    ));
                }
            }
        }

        const DEFAULT_EVENT_LIMIT: u32 = 50;
        let limit = request.limit.unwrap_or(DEFAULT_EVENT_LIMIT);
        let request = driver_req::ReadEventsRequest {
            event_type: request.event_type,
            state_key: request.state_key,
            limit,
        };

        self.send_matrix_driver_request(request).map(|(request, action)| {
            request.add_response_handler(|result, machine| {
                machine.send_read_events_response(raw_request, result)
            });
            action
        })
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

        request.add_response_handler(|mut result, machine| {
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

    fn process_to_device_request(
        &mut self,
        request: SendToDeviceRequest,
        raw_request: Raw<FromWidgetRequest>,
    ) -> Option<Action> {
        let CapabilitiesState::Negotiated(capabilities) = &self.capabilities else {
            error!("Received send event request before capabilities negotiation");
            return None;
        };

        if !capabilities.allow_sending(&request) {
            return Some(Self::send_from_widget_error_string_response(
                raw_request,
                format!("Not allowed to send to-device message of type: {}", request.event_type),
            ));
        }

        let (request, action) = self.send_matrix_driver_request(request)?;
        request.add_response_handler(|result, _| {
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
                let mut object = raw_request
                    .deserialize_as_unchecked::<IndexMap<String, Box<RawJsonValue>>>()?;
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
            warn!("Reached limits of pending requests for Matrix driver requests");
            return None;
        };

        Some((
            MatrixDriverRequestHandle::new(meta),
            Action::MatrixDriverRequest { request_id, data: request.into() },
        ))
    }

    /// Sends a [`NotifyStateUpdate`] action to the widget. The `events`
    /// indicate which room state entries (may) have changed.
    fn send_state_update(&mut self, events: Vec<Raw<AnyStateEvent>>) -> Option<Action> {
        self.send_to_widget_request(NotifyStateUpdate { state: events })
            .map(|(_request, action)| action)
    }

    /// Processes a response to one of the read state requests sent to the
    /// [`crate::widget::MatrixDriver`] to compute an initial state update. If
    /// the update is complete, this will send it off to the widget in a
    /// [`NotifyStateUpdate`] action.
    fn process_read_initial_state_response(
        &mut self,
        events: Vec<Raw<AnyStateEvent>>,
    ) -> Option<Vec<Action>> {
        // Pull the updates struct out of the machine temporarily so that we can match
        // on it in one place, mutate it, and still be able to call
        // `send_to_widget_request` later in this block (which borrows the machine
        // mutably)
        match self.pending_state_updates.take() {
            None => {
                error!(
                    "Initial state updates must only be set to `None` once all requests \
                     are complete; dropping state response"
                );
                None
            }

            Some(mut updates) => {
                updates.initial_state.push(events);

                if updates.initial_state.len() != updates.request_count {
                    // Not all of the initial state requests have completed yet; put the updates
                    // struct back so we can continue accumulating the initial state.
                    self.pending_state_updates = Some(updates);
                    return None;
                }

                // The initial state is complete; combine the data and push it to the widget in
                // a single action.
                let initial =
                    self.send_state_update(updates.initial_state.into_iter().flatten().collect());
                // Also flush any state updates that had been postponed until after the initial
                // state push. We deliberately do not bundle these updates into a single action,
                // since they might contain some repeated updates to the same room state entry
                // which could confuse the widget if included in the same `events` array. It's
                // easiest to let the widget process each update sequentially rather than put
                // effort into coalescing them - this is for an edge case after all.
                let postponed = updates
                    .postponed_updates
                    .into_iter()
                    .map(|state| self.send_state_update(state));

                // The postponed updates should come after the initial update
                Some(initial.into_iter().chain(postponed.flatten()).collect())
            }
        }
    }

    /// Processes a response from the [`crate::widget::MatrixDriver`] saying
    /// that the widget is approved to acquire some capabilities. This will
    /// store those capabilities in the state machine, notify the widget,
    /// and then begin computing an initial state update if the widget
    /// was approved to read room state.
    fn process_acquired_capabilities(
        &mut self,
        approved: Result<Capabilities, Error>,
        requested: Capabilities,
    ) -> Vec<Action> {
        let approved = approved.unwrap_or_else(|e| {
            error!("Acquiring capabilities failed: {e}");
            Capabilities::default()
        });

        let mut actions = Vec::new();
        if !approved.read.is_empty() {
            actions.push(Action::Subscribe);
        }

        self.capabilities = CapabilitiesState::Negotiated(approved.clone());

        let state_filters: Vec<_> = approved
            .read
            .iter()
            .filter_map(|f| match f {
                Filter::State(f) => Some(f),
                _ => None,
            })
            .cloned()
            .collect();

        if !state_filters.is_empty() {
            // Begin accumulating the initial state to be pushed to the widget. Since this
            // widget driver currently doesn't implement capability
            // renegotiation, we can be sure that we aren't overwriting another
            // in-progress update.
            if self.pending_state_updates.is_some() {
                // Or so we should be. Let's at least log something if we ever break that
                // invariant.
                error!("Another initial state update is in progress; overwriting it");
            }
            self.pending_state_updates = Some(InitialStateUpdate {
                initial_state: Vec::with_capacity(state_filters.len()),
                request_count: state_filters.len(),
                postponed_updates: Vec::new(),
            })
        }

        // For each room state filter that the widget has been approved to read, fire
        // off a request to the driver to determine the initial values of the
        // matching room state entries
        let initial_state_actions = state_filters.iter().flat_map(|filter| {
            self.send_matrix_driver_request(match filter {
                StateEventFilter::WithType(event_type) => ReadStateRequest {
                    event_type: event_type.to_string(),
                    state_key: StateKeySelector::Any,
                },
                StateEventFilter::WithTypeAndStateKey(event_type, state_key) => ReadStateRequest {
                    event_type: event_type.to_string(),
                    state_key: StateKeySelector::Key(state_key.clone()),
                },
            })
            .map(|(request, action)| {
                request.add_response_handler(move |result, machine| {
                    machine
                        .process_read_initial_state_response(result.unwrap_or_else(|e| {
                            error!("Reading initial room state failed: {e}");
                            // Pretend that we just got an empty response so the initial state
                            // update won't be completely blocked on this one bit of missing data
                            Vec::new()
                        }))
                        .unwrap_or_default()
                });
                action
            })
        });

        actions.extend(initial_state_actions);

        let notify_caps_changed = NotifyCapabilitiesChanged { approved, requested };
        if let Some((_request, action)) = self.send_to_widget_request(notify_caps_changed) {
            actions.push(action);
        }

        actions
    }

    /// Attempts to acquire capabilities that have been requested by the widget
    /// during the initial capability negotiation handshake.
    fn process_requested_capabilities(&mut self, requested: Capabilities) -> Vec<Action> {
        match self.send_matrix_driver_request(AcquireCapabilities {
            desired_capabilities: requested.clone(),
        }) {
            None => Vec::new(),
            Some((request, action)) => {
                request.add_response_handler(|result, machine| {
                    machine.process_acquired_capabilities(result, requested)
                });
                vec![action]
            }
        }
    }

    /// Performs an initial capability negotiation handshake.
    ///
    /// The sequence is as follows: the machine sends a [`RequestCapabilities`]
    /// `toWidget` action, the widget responds with its requested
    /// capabilities, the machine attempts to acquire the
    /// requested capabilities from the driver, then it sends a
    /// [`NotifyCapabilitiesChanged`] `toWidget` action to tell the widget
    /// which capabilities were approved.
    fn negotiate_capabilities(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();

        // XXX: This branch appears to be accounting for capability **re**negotiation
        // (MSC2974), which isn't implemented yet
        if matches!(&self.capabilities, CapabilitiesState::Negotiated(c) if !c.read.is_empty()) {
            actions.push(Action::Unsubscribe);
        }

        self.capabilities = CapabilitiesState::Negotiating;

        if let Some((request, action)) = self.send_to_widget_request(RequestCapabilities {}) {
            request.add_response_handler(|result, machine| {
                machine.process_requested_capabilities(result.capabilities)
            });
            actions.push(action);
        }

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
