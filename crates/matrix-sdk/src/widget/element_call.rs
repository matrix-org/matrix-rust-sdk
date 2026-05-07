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

//! Element Call specific widget helpers.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use ruma::{
    events::call::member::{
        ActiveFocus, ActiveLivekitFocus, Application, CallApplicationContent,
        CallMemberEventContent, CallMemberStateKey, CallScope, Focus, LivekitFocus,
    },
    events::{MessageLikeEventType, StateEventType},
    DeviceId, RoomId, UserId,
};
use serde::Serialize;
use serde_json::{json, Value as JsonValue};
use tokio::sync::{oneshot, watch};
use tracing::info;

use super::{
    Capabilities, ClientProperties, EncryptionSystem, Filter, Intent, MessageLikeEventFilter,
    StateEventFilter, StaticCapabilitiesProvider, ToDeviceEventFilter,
    VirtualElementCallWidgetConfig, VirtualElementCallWidgetProperties, WidgetDriver,
    WidgetDriverHandle, WidgetSettings,
};
use crate::{room::Room, Error, Result};

/// Runtime handle and state for a started Element Call widget.
#[derive(Clone, Debug)]
pub struct ElementCallWidget {
    handle: WidgetDriverHandle,
    widget_id: String,
    capabilities_ready: watch::Receiver<bool>,
    pending_widget_responses: Arc<Mutex<HashMap<String, oneshot::Sender<JsonValue>>>>,
}

impl ElementCallWidget {
    /// Access the driver handle used to send/receive widget messages.
    pub fn handle(&self) -> &WidgetDriverHandle {
        &self.handle
    }

    /// The widget id used by this widget instance.
    pub fn widget_id(&self) -> &str {
        &self.widget_id
    }

    /// A watch channel that flips to `true` after capability negotiation.
    pub fn capabilities_ready(&self) -> watch::Receiver<bool> {
        self.capabilities_ready.clone()
    }

    /// Track a request id and return a receiver for the corresponding widget response.
    pub fn track_pending_response(&self, request_id: String) -> oneshot::Receiver<JsonValue> {
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending_widget_responses.lock() {
            pending.insert(request_id, tx);
        }
        rx
    }

    /// Stop tracking a request id.
    pub fn remove_pending_response(&self, request_id: &str) {
        if let Ok(mut pending) = self.pending_widget_responses.lock() {
            pending.remove(request_id);
        }
    }
}

/// Additional options used to initialize a virtual Element Call widget.
#[derive(Debug)]
pub struct ElementCallWidgetOptions {
    /// The widget id used in widget-api messages.
    pub widget_id: String,
    /// The parent URL used as postMessage target.
    pub parent_url: Option<String>,
    /// Encryption mode for Element Call.
    pub encryption: EncryptionSystem,
    /// Join/start intent for Element Call.
    pub intent: Intent,
    /// Client properties used for URL generation.
    pub client_properties: ClientProperties,
}

impl Default for ElementCallWidgetOptions {
    fn default() -> Self {
        Self {
            widget_id: "element-call".to_owned(),
            parent_url: None,
            encryption: EncryptionSystem::PerParticipantKeys,
            intent: Intent::JoinExisting,
            client_properties: ClientProperties::new("matrix-rust-sdk", None, None),
        }
    }
}

/// Start a virtual Element Call widget backed by a [`WidgetDriver`].
pub async fn start_element_call_widget(
    room: Room,
    element_call_url: String,
    options: ElementCallWidgetOptions,
) -> Result<ElementCallWidget> {
    let own_user_id = room
        .client()
        .user_id()
        .ok_or_else(|| {
            Error::UnknownError(
                std::io::Error::other("missing user id for element call widget").into(),
            )
        })?
        .to_owned();
    let own_device_id = room
        .client()
        .device_id()
        .ok_or_else(|| {
            Error::UnknownError(
                std::io::Error::other("missing device id for element call widget").into(),
            )
        })?
        .to_owned();

    let props = VirtualElementCallWidgetProperties {
        element_call_url,
        widget_id: options.widget_id,
        parent_url: options.parent_url,
        encryption: options.encryption,
        ..Default::default()
    };
    let config =
        VirtualElementCallWidgetConfig { intent: Some(options.intent), ..Default::default() };

    let widget_settings = WidgetSettings::new_virtual_element_call_widget(props, config)?;
    let widget_id = widget_settings.widget_id().to_owned();
    let widget_url = widget_settings.generate_webview_url(&room, options.client_properties).await?;
    info!(%widget_url, "element call widget url");
    info!(widget_id = %widget_settings.widget_id(), "starting Element Call widget driver");

    let (driver, handle) = WidgetDriver::new(widget_settings);
    let capabilities = element_call_capabilities(&own_user_id, &own_device_id);
    let capabilities_provider = StaticCapabilitiesProvider::new(capabilities);
    let widget_capabilities = capabilities_provider.capabilities().clone();
    let (capabilities_ready_tx, capabilities_ready_rx) = watch::channel(false);
    let pending_widget_responses: Arc<Mutex<HashMap<String, oneshot::Sender<JsonValue>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    tokio::spawn(async move {
        if driver.run(room, capabilities_provider).await.is_err() {
            info!("element call widget driver stopped");
        }
    });

    let outbound_handle = handle.clone();
    let outbound_widget_id = widget_id.clone();
    let pending_widget_responses_for_task = pending_widget_responses.clone();
    tokio::spawn(async move {
        let capabilities_ready_tx = capabilities_ready_tx;
        let pending_widget_responses = pending_widget_responses_for_task;
        while let Some(message) = outbound_handle.recv().await {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&message) else {
                continue;
            };
            let Some(request_id) = value.get("requestId").and_then(|v| v.as_str()) else {
                continue;
            };
            if value.get("response").is_some() {
                if let Some(tx) = pending_widget_responses
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.remove(request_id))
                {
                    let _ = tx.send(value);
                }
                continue;
            }
            let Some(action) = value.get("action").and_then(|v| v.as_str()) else {
                continue;
            };
            let api = value.get("api").and_then(|v| v.as_str());
            if api != Some("toWidget") {
                continue;
            }
            if action == "capabilities" {
                let response = serde_json::json!({
                    "api": "toWidget",
                    "widgetId": outbound_widget_id,
                    "requestId": request_id,
                    "action": "capabilities",
                    "data": {},
                    "response": {
                        "capabilities": widget_capabilities,
                    },
                });
                if !outbound_handle.send(response.to_string()).await {
                    break;
                }
            }
            if action == "notify_capabilities" {
                let response = serde_json::json!({
                    "api": "toWidget",
                    "widgetId": outbound_widget_id,
                    "requestId": request_id,
                    "action": "notify_capabilities",
                    "data": {},
                    "response": {},
                });
                if !outbound_handle.send(response.to_string()).await {
                    break;
                }
                let _ = capabilities_ready_tx.send(true);
            }
            if action == "send_to_device" {
                let data = value.get("data").cloned().unwrap_or_else(|| serde_json::json!({}));
                let event_type = data
                    .get("event_type")
                    .and_then(|v| v.as_str())
                    .or_else(|| data.get("type").and_then(|v| v.as_str()));
                if event_type == Some("io.element.call.encryption_keys") {
                    info!(request_id, payload = %data, "widget send_to_device encryption key payload");
                } else {
                    info!(request_id, event_type, "widget send_to_device received");
                }
            }
        }
        info!("widget -> rust-sdk message stream closed");
    });

    let content_loaded = serde_json::json!({
        "api": "fromWidget",
        "widgetId": widget_id,
        "requestId": format!(
            "content-loaded-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
        ),
        "action": "content_loaded",
        "data": {},
    });
    let _ = handle.send(content_loaded.to_string()).await;

    Ok(ElementCallWidget {
        handle,
        widget_id,
        capabilities_ready: capabilities_ready_rx,
        pending_widget_responses,
    })
}

/// Build the default Element Call capability set used by SDK-driven widgets.
pub fn element_call_capabilities(own_user_id: &UserId, own_device_id: &DeviceId) -> Capabilities {
    let read_send = vec![
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::from(
            "org.matrix.rageshake_request",
        ))),
        Filter::ToDevice(ToDeviceEventFilter::new("io.element.call.encryption_keys".into())),
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::from(
            "io.element.call.encryption_keys",
        ))),
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::from(
            "io.element.call.reaction",
        ))),
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::Reaction)),
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::RoomRedaction)),
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::RtcDecline)),
    ];

    let user_id = own_user_id.as_str();
    let device_id = own_device_id.as_str();
    let membership_state_key = CallMemberStateKey::new(
        own_user_id.to_owned(),
        Some(format!("{own_device_id}_m.call")),
        false,
    )
    .as_ref()
    .to_owned();

    Capabilities {
        read: vec![
            Filter::State(StateEventFilter::WithType(StateEventType::CallMember)),
            Filter::State(StateEventFilter::WithType(StateEventType::RoomName)),
            Filter::State(StateEventFilter::WithType(StateEventType::RoomMember)),
            Filter::State(StateEventFilter::WithType(StateEventType::RoomEncryption)),
            Filter::State(StateEventFilter::WithType(StateEventType::RoomCreate)),
        ]
        .into_iter()
        .chain(read_send.clone())
        .collect(),
        send: vec![
            Filter::MessageLike(MessageLikeEventFilter::WithType(
                MessageLikeEventType::RtcNotification,
            )),
            Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::CallNotify)),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                user_id.to_owned(),
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                format!("{user_id}_{device_id}"),
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                membership_state_key,
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                format!("{user_id}_{device_id}_m.call"),
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                format!("_{user_id}_{device_id}"),
            )),
            Filter::State(StateEventFilter::WithTypeAndStateKey(
                StateEventType::CallMember,
                format!("_{user_id}_{device_id}_m.call"),
            )),
        ]
        .into_iter()
        .chain(read_send)
        .collect(),
        requires_client: true,
        update_delayed_event: true,
        send_delayed_event: true,
    }
}

/// Build `m.call.member` content suitable for publishing through the widget API.
pub fn element_call_member_content(
    room_id: &RoomId,
    own_device_id: &DeviceId,
    service_url: &str,
) -> CallMemberEventContent {
    let application =
        Application::Call(CallApplicationContent::new("".to_owned(), CallScope::Room));
    let focus_active = ActiveFocus::Livekit(ActiveLivekitFocus::new());
    let foci_preferred =
        vec![Focus::Livekit(LivekitFocus::new(room_id.to_string(), service_url.to_owned()))];

    CallMemberEventContent::new(
        application,
        own_device_id.to_owned(),
        focus_active,
        foci_preferred,
        None,
        None,
    )
}

/// Build a `fromWidget` `send_event` request payload for `m.call.member`.
pub fn element_call_send_event_message(
    widget_id: &str,
    request_id: &str,
    state_key: &str,
    content: &impl Serialize,
) -> JsonValue {
    json!({
        "api": "fromWidget",
        "widgetId": widget_id,
        "requestId": request_id,
        "action": "send_event",
        "data": {
            "type": "org.matrix.msc3401.call.member",
            "state_key": state_key,
            "content": content,
        },
    })
}

/// Publish `m.call.member` through the widget API for the current user/device.
pub async fn publish_call_membership_via_widget(
    room: Room,
    widget: &ElementCallWidget,
    service_url: &str,
) -> Result<()> {
    if !*widget.capabilities_ready().borrow() {
        let mut capabilities_ready = widget.capabilities_ready();
        let _ = capabilities_ready.changed().await;
    }

    let own_user_id = room
        .client()
        .user_id()
        .ok_or_else(|| {
            Error::UnknownError(
                std::io::Error::other("missing user id for widget membership publisher").into(),
            )
        })?
        .to_owned();
    let own_device_id = room
        .client()
        .device_id()
        .ok_or_else(|| {
            Error::UnknownError(
                std::io::Error::other("missing device id for widget membership publisher").into(),
            )
        })?
        .to_owned();

    let state_key =
        CallMemberStateKey::new(own_user_id.clone(), Some(own_device_id.to_string()), true);
    let content = element_call_member_content(room.room_id(), &own_device_id, service_url);
    let request_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_owned());
    let send_event_message = element_call_send_event_message(
        widget.widget_id(),
        &request_id,
        state_key.as_ref(),
        &content,
    );

    let send_event_message_json = send_event_message.to_string();
    info!(
        request_body = send_event_message_json.as_str(),
        "Publishing MatrixRTC membership send_event via widget api"
    );

    if !widget.handle().send(send_event_message.to_string()).await {
        return Err(Error::UnknownError(
            std::io::Error::other(
                "widget driver handle closed before sending membership send_event",
            )
            .into(),
        ));
    }

    info!(state_key = state_key.as_ref(), "published MatrixRTC membership via widget api");
    Ok(())
}

/// Send an empty `m.call.member` event through the widget API to shut down membership.
pub async fn send_hangup_via_widget(
    widget: &ElementCallWidget,
    state_key: Option<&CallMemberStateKey>,
) -> Result<()> {
    const SHUTDOWN_WIDGET_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    if !*widget.capabilities_ready().borrow() {
        let mut capabilities_ready = widget.capabilities_ready();
        let _ =
            tokio::time::timeout(SHUTDOWN_WIDGET_WAIT_TIMEOUT, capabilities_ready.changed()).await;
    }

    let request_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_owned());
    let response_rx = widget.track_pending_response(request_id.clone());
    let state_key = state_key.map(|state_key| state_key.as_ref()).unwrap_or_default();
    let shutdown_message = element_call_send_event_message(
        widget.widget_id(),
        &request_id,
        state_key,
        &serde_json::json!({}),
    );
    info!(
        request_body = shutdown_message.to_string().as_str(),
        "sending shutdown membership send_event via widget api"
    );

    match tokio::time::timeout(
        SHUTDOWN_WIDGET_WAIT_TIMEOUT,
        widget.handle().send(shutdown_message.to_string()),
    )
    .await
    {
        Ok(true) => info!("shutdown membership send_event sent via widget api"),
        Ok(false) => {
            widget.remove_pending_response(&request_id);
            return Err(Error::UnknownError(
                std::io::Error::other(
                    "widget driver handle closed before sending shutdown membership send_event",
                )
                .into(),
            ));
        }
        Err(_) => {
            widget.remove_pending_response(&request_id);
            info!(
                "timeout while sending shutdown membership send_event via widget api; continuing shutdown"
            );
            return Ok(());
        }
    }

    match tokio::time::timeout(SHUTDOWN_WIDGET_WAIT_TIMEOUT, response_rx).await {
        Ok(Ok(_response)) => {
            info!(request_id, "received widget response for shutdown membership send_event")
        }
        Ok(Err(_)) => info!(
            request_id,
            "shutdown membership send_event response channel closed; continuing shutdown"
        ),
        Err(_) => {
            widget.remove_pending_response(&request_id);
            info!(
                request_id,
                "timeout waiting for widget shutdown membership send_event response; continuing shutdown"
            );
        }
    }

    Ok(())
}
