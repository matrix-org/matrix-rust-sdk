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

use indexmap::{map::Entry, IndexMap};
use ruma::serde::{JsonObject, Raw};
use serde::Serialize;
use serde_json::value::RawValue as RawJsonValue;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{error, instrument, trace, warn};
use uuid::Uuid;

use self::{
    driver_req::{AcquireCapabilities, MatrixDriverRequest, MatrixDriverRequestHandle},
    from_widget::{FromWidgetErrorResponse, FromWidgetRequest, SupportedApiVersionsResponse},
    incoming::{IncomingWidgetMessage, IncomingWidgetMessageKind},
    to_widget::{
        NotifyPermissionsChanged, RequestPermissions, ToWidgetRequest, ToWidgetRequestHandle,
        ToWidgetResponse,
    },
};

mod actions;
mod driver_req;
mod from_widget;
mod incoming;
mod openid;
#[cfg(test)]
mod tests;
mod to_widget;

pub(crate) use self::{
    actions::{Action, MatrixDriverRequestData, SendEventCommand},
    incoming::{IncomingMessage, MatrixDriverResponse},
};
use super::Capabilities;
#[cfg(doc)]
use super::WidgetDriver;

/// No I/O state machine.
///
/// Handles interactions with the widget as well as the `MatrixDriver`.
pub(crate) struct WidgetMachine {
    widget_id: String,
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
        init_on_content_load: bool,
    ) -> (Self, UnboundedReceiver<Action>) {
        let (actions_sender, actions_receiver) = unbounded_channel();
        let mut machine = Self {
            widget_id,
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
            IncomingMessage::MatrixEventReceived(_) => {
                error!("processing incoming matrix events not yet implemented");
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
                self.process_from_widget_request(request);
            }
            IncomingWidgetMessageKind::Response(response) => {
                self.process_to_widget_response(message.request_id, response);
            }
        }
    }

    #[instrument(skip_all, fields(request_id))]
    fn process_from_widget_request(&mut self, raw_request: Raw<FromWidgetRequest>) {
        let request = match raw_request.deserialize() {
            Ok(r) => r,
            Err(e) => {
                self.send_from_widget_response(raw_request, FromWidgetErrorResponse::new(e));
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
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                error!("Matrix driver request failed: {e}");
                return;
            }
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
        self.capabilities = CapabilitiesState::Negotiating;

        self.send_to_widget_request(RequestPermissions {})
            // TODO: Each request can actually fail here, take this into an account.
            .then(|desired_capabilities, machine| {
                machine
                    .send_matrix_driver_request(AcquireCapabilities {
                        desired_capabilities: desired_capabilities.clone(),
                    })
                    .then(|granted_capabilities, machine| {
                        machine.capabilities =
                            CapabilitiesState::Negotiated(granted_capabilities.clone());
                        machine.send_to_widget_request(NotifyPermissionsChanged {
                            approved: granted_capabilities,
                            requested: desired_capabilities,
                        });
                    })
            });
    }
}

type ToWidgetResponseFn = Box<dyn FnOnce(Box<RawJsonValue>, &mut WidgetMachine) + Send>;

pub(crate) struct ToWidgetRequestMeta {
    #[allow(dead_code)]
    action: &'static str,
    response_fn: Option<ToWidgetResponseFn>,
}

impl ToWidgetRequestMeta {
    fn new(action: &'static str) -> Self {
        Self { action, response_fn: None }
    }
}

type MatrixDriverResponseFn = Box<dyn FnOnce(MatrixDriverResponse, &mut WidgetMachine) + Send>;

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
