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
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue as RawJsonValue;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{error, instrument};
use uuid::Uuid;

use self::{
    driver_req::{AcquirePermissions, MatrixDriverRequest, MatrixDriverRequestHandle},
    to_widget::{
        NotifyPermissionsChanged, RequestPermissions, ToWidgetRequest, ToWidgetRequestHandle,
    },
};

mod actions;
mod driver_req;
mod events;
mod openid;
#[cfg(test)]
mod tests;
mod to_widget;

pub(crate) use self::{
    actions::{Action, MatrixDriverRequestData, MatrixDriverResponse, SendEventCommand},
    events::Event,
};
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
        };

        if !init_on_content_load {
            machine
                .send_to_widget_request(RequestPermissions {})
                // TODO: Each request can actually fail here, take this into an account.
                .then(|desired_permissions, machine| {
                    machine
                        .send_matrix_driver_request(AcquirePermissions {
                            desired_permissions: desired_permissions.clone(),
                        })
                        .then(|granted_permissions, machine| {
                            machine.send_to_widget_request(NotifyPermissionsChanged {
                                approved: granted_permissions,
                                requested: desired_permissions,
                            });
                        })
                });
        }

        (machine, actions_receiver)
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

    /// Processes an incoming event (an incoming raw message from a widget,
    /// or a data produced as a result of a previously sent `Action`).
    /// Produceses a list of actions that the client must perform.
    pub(crate) fn process(&mut self, event: Event) {
        // For now we assume that we only receive responses to the outgoing requests.
        // TODO: Add other possible valid incoming messages.
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct IncomingMessage {
            request_id: Uuid,
            widget_id: String,
            response: Box<RawJsonValue>,
        }

        match event {
            Event::MessageFromWidget(raw) => {
                let message = match serde_json::from_str::<IncomingMessage>(&raw) {
                    Ok(msg) => msg,
                    Err(e) => {
                        error!("Failed to deserialize incoming message: {e}");
                        return;
                    }
                };

                if message.widget_id != self.widget_id {
                    error!("Received a message from a wrong widget");
                    return;
                }

                self.pending_to_widget_requests
                    .remove(&message.request_id)
                    .and_then(|meta| meta.response_fn)
                    .map(|response_fn| response_fn(message.response, self));
            }

            Event::PermissionsAcquired(response) => {
                self.pending_matrix_driver_requests
                    .remove(&response.request_id)
                    .and_then(|meta| meta.response_fn)
                    .map(|response_fn| response_fn(Event::PermissionsAcquired(response), self));
            }

            _ => {
                error!("Dropping an event as it's not supported yet");
            }
        }
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

type MatrixDriverResponseFn = Box<dyn FnOnce(Event, &mut WidgetMachine) + Send>;

pub(crate) struct MatrixDriverRequestMeta {
    response_fn: Option<MatrixDriverResponseFn>,
}

impl MatrixDriverRequestMeta {
    fn new() -> Self {
        Self { response_fn: None }
    }
}
