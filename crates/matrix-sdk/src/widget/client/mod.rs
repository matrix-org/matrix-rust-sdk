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

//! Internal client widget API implementation.

use std::{
    borrow::Cow,
    sync::{Arc, Mutex},
};

use ruma::OwnedRoomId;
use serde_json::{from_str as from_json, to_string as to_json};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::error;

use self::internal::{
    incoming::{
        ContentLoadedRequest, GetOpenIdRequest, GetSupportedApiVersionsRequest, HandlerContext,
        ReadEventRequest, RequestHandler, SendEventRequest,
    },
    messages::{
        from_widget::{RequestType, ResponseType},
        IncomingMessage, IncomingMessageKind, OutgoingMessage,
    },
    outgoing::{AcquirePermissions, CommandProxy, RequestPermissions, UpdatePermissions},
};
pub use self::{
    actions::{Action, SendEventCommand},
    events::Event,
};
use super::Permissions;

mod actions;
mod events;
mod internal;

/// State machine that handles the client widget API interractions.
pub struct ClientApi {
    settings: ClientApiSettings,
    permissions: Arc<Mutex<Option<Permissions>>>,
    command_proxy: Arc<CommandProxy>,
    actions_tx: UnboundedSender<Action>,
}

/// Settings for the client widget API state machine.
#[derive(Clone, Debug)]
pub struct ClientApiSettings {
    pub init_on_content_load: bool,
    pub room_id: OwnedRoomId,
}

impl ClientApi {
    /// Creates a new instance of a client widget API state machine.
    /// Returns the client api handler as well as the channel to receive
    /// actions (commands) from the client.
    pub fn new(settings: ClientApiSettings) -> (Self, UnboundedReceiver<Action>) {
        let permissions = Arc::new(Mutex::new(None));
        let command_proxy = Arc::new(CommandProxy);

        // Unless we're asked to wait for the content load message,
        // we must start the negotiation of permissions right away.
        if !settings.init_on_content_load {
            let (perm, proxy) = (permissions.clone(), command_proxy.clone());
            tokio::spawn(async move {
                // TODO: Handle an error.
                if let Ok(negotiated) = negotiate_permissions(proxy).await {
                    perm.lock().unwrap().replace(negotiated);
                }
            });
        }

        let (actions_tx, actions_rx) = unbounded_channel();
        (Self { settings, permissions, command_proxy, actions_tx }, actions_rx)
    }

    /// Processes an incoming event (an incoming raw message from a widget,
    /// or a data produced as a result of a previously sent `Action`).
    pub fn process(&mut self, event: Event) {
        match event {
            Event::MessageFromWidget(raw) => match from_json::<IncomingMessage>(&raw) {
                Ok(msg) => match msg.kind {
                    IncomingMessageKind::FromWidget(request_type) => {
                        let ctx = HandlerContext::new(
                            self.permissions.clone(),
                            self.settings.clone(),
                            self.command_proxy.clone(),
                        );
                        let id = msg.header.request_id.clone();
                        let actions_tx = self.actions_tx.clone();
                        tokio::spawn(async move {
                            let response = process_incoming_request(ctx, id, request_type).await;
                            let msg = OutgoingMessage::response(msg.header, response);
                            let raw = to_json(&msg).expect("Failed to serialize a message");
                            let _ = actions_tx.send(Action::SendToWidget(raw));
                        });
                    }
                    IncomingMessageKind::ToWidget(_) => {}
                },
                Err(err) => {
                    // TODO: Properly handle this error by sending an error response.
                    error!("Failed to parse a message from a widget: {}", err);
                }
            },
            Event::MatrixEventReceived(_) => {}
            Event::PermissionsAcquired(_) => {}
            Event::OpenIdReceived(_) => {}
            Event::MatrixEventRead(_) => {}
            Event::MatrixEventSent(_) => {}
        }
    }
}

async fn process_incoming_request(
    ctx: HandlerContext,
    id: String,
    request_type: RequestType,
) -> ResponseType {
    match request_type {
        RequestType::GetSupportedApiVersion(r) => ResponseType::GetSupportedApiVersion(
            r.clone().map(GetSupportedApiVersionsRequest::handle(ctx, id, r.content).await),
        ),
        RequestType::ContentLoaded(r) => ResponseType::ContentLoaded(
            r.clone().map(ContentLoadedRequest::handle(ctx, id, r.content).await),
        ),
        RequestType::GetOpenId(r) => ResponseType::GetOpenId(
            r.clone().map(GetOpenIdRequest::handle(ctx, id, r.content).await),
        ),
        RequestType::ReadEvent(r) => ResponseType::ReadEvent(
            r.clone().map(ReadEventRequest::handle(ctx, id, r.content).await),
        ),
        RequestType::SendEvent(r) => ResponseType::SendEvent(
            r.clone().map(SendEventRequest::handle(ctx, id, r.content).await),
        ),
    }
}

async fn negotiate_permissions(proxy: Arc<CommandProxy>) -> Result<Permissions, Cow<'static, str>> {
    let desired_permissions = proxy.send(RequestPermissions).await?;
    let granted_permissions = proxy.send(AcquirePermissions(desired_permissions)).await?;
    proxy.send(UpdatePermissions(granted_permissions.clone())).await?;
    Ok(granted_permissions)
}
