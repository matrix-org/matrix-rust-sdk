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

//! Matrix driver implementation that exposes Matrix functionality
//! that is relevant for the widget API.

use std::collections::{BTreeMap, BTreeSet};

use as_variant::as_variant;
use matrix_sdk_base::{
    crypto::CollectStrategy,
    deserialized_responses::{EncryptionInfo, RawAnySyncOrStrippedState},
    sync::State,
};
use ruma::{
    EventId, OwnedDeviceId, OwnedUserId, RoomId, TransactionId,
    api::client::{
        account::request_openid_token::v3::{Request as OpenIdRequest, Response as OpenIdResponse},
        delayed_events::{self, update_delayed_event::unstable::UpdateAction},
        filter::RoomEventFilter,
        to_device::send_event_to_device::v3::Request as RumaToDeviceRequest,
    },
    assign,
    events::{
        AnyMessageLikeEventContent, AnyStateEvent, AnyStateEventContent, AnySyncStateEvent,
        AnySyncTimelineEvent, AnyTimelineEvent, AnyToDeviceEvent, AnyToDeviceEventContent,
        MessageLikeEventType, StateEventType, TimelineEventType, ToDeviceEventType,
    },
    serde::{Raw, from_raw_json_value},
    to_device::DeviceIdOrAllDevices,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue as RawJsonValue};
use tokio::sync::{
    broadcast::{Receiver, error::RecvError},
    mpsc::{UnboundedReceiver, unbounded_channel},
};
use tracing::{error, trace, warn};

use super::{StateKeySelector, machine::SendEventResponse};
use crate::{
    Client, Error, Result, Room, event_handler::EventHandlerDropGuard, room::MessagesOptions,
    sync::RoomUpdate, widget::machine::SendToDeviceEventResponse,
};

/// Thin wrapper around a [`Room`] that provides functionality relevant for
/// widgets.
pub(crate) struct MatrixDriver {
    room: Room,
}

impl MatrixDriver {
    /// Creates a new `MatrixDriver` for a given `room`.
    pub(crate) fn new(room: Room) -> Self {
        Self { room }
    }

    /// Requests an OpenID token for the current user.
    pub(crate) async fn get_open_id(&self) -> Result<OpenIdResponse> {
        let user_id = self.room.own_user_id().to_owned();
        self.room
            .client
            .send(OpenIdRequest::new(user_id))
            .await
            .map_err(|error| Error::Http(Box::new(error)))
    }

    /// Reads the latest `limit` events of a given `event_type` from the room's
    /// timeline.
    pub(crate) async fn read_events(
        &self,
        event_type: TimelineEventType,
        state_key: Option<StateKeySelector>,
        limit: u32,
    ) -> Result<Vec<Raw<AnyTimelineEvent>>> {
        let options = assign!(MessagesOptions::backward(), {
            limit: limit.into(),
            filter: assign!(RoomEventFilter::default(), {
                types: Some(vec![event_type.to_string()])
            }),
        });

        let messages = self.room.messages(options).await?;

        Ok(messages
            .chunk
            .into_iter()
            .map(|ev| ev.into_raw().cast_unchecked())
            .filter(|ev| match &state_key {
                Some(state_key) => {
                    ev.get_field::<String>("state_key").is_ok_and(|key| match state_key {
                        StateKeySelector::Key(state_key) => {
                            key.is_some_and(|key| &key == state_key)
                        }
                        StateKeySelector::Any => key.is_some(),
                    })
                }
                None => true,
            })
            .collect())
    }

    /// Reads the current values of the room state entries matching the given
    /// `event_type` and `state_key` selections.
    pub(crate) async fn read_state(
        &self,
        event_type: StateEventType,
        state_key: &StateKeySelector,
    ) -> Result<Vec<Raw<AnyStateEvent>>> {
        let room_id = self.room.room_id();
        let convert = |sync_or_stripped_state| match sync_or_stripped_state {
            RawAnySyncOrStrippedState::Sync(ev) => Some(attach_room_id_state(&ev, room_id)),
            RawAnySyncOrStrippedState::Stripped(_) => {
                error!("MatrixDriver can't operate in invited rooms");
                None
            }
        };

        let events = match state_key {
            StateKeySelector::Key(state_key) => self
                .room
                .get_state_event(event_type, state_key)
                .await?
                .and_then(convert)
                .into_iter()
                .collect(),
            StateKeySelector::Any => {
                let events = self.room.get_state_events(event_type).await?;
                events.into_iter().filter_map(convert).collect()
            }
        };

        Ok(events)
    }

    /// Sends the given `event` to the room.
    ///
    /// This method allows the widget machine to handle widget requests by
    /// providing a unified, high-level widget-specific API for sending events
    /// to the room.
    pub(crate) async fn send(
        &self,
        event_type: TimelineEventType,
        state_key: Option<String>,
        content: Box<RawJsonValue>,
        delayed_event_parameters: Option<delayed_events::DelayParameters>,
    ) -> Result<SendEventResponse> {
        let type_str = event_type.to_string();

        if let Some(redacts) = from_raw_json_value::<Value, serde_json::Error>(&content)
            .ok()
            .and_then(|b| b["redacts"].as_str().and_then(|s| EventId::parse(s).ok()))
        {
            return Ok(SendEventResponse::from_event_id(
                self.room.redact(&redacts, None, None).await?.event_id,
            ));
        }

        Ok(match (state_key, delayed_event_parameters) {
            (None, None) => SendEventResponse::from_event_id(
                self.room.send_raw(&type_str, content).await?.response.event_id,
            ),

            (Some(key), None) => SendEventResponse::from_event_id(
                self.room.send_state_event_raw(&type_str, &key, content).await?.event_id,
            ),

            (None, Some(delayed_event_parameters)) => {
                let r = delayed_events::delayed_message_event::unstable::Request::new_raw(
                    self.room.room_id().to_owned(),
                    TransactionId::new(),
                    MessageLikeEventType::from(type_str),
                    delayed_event_parameters,
                    Raw::<AnyMessageLikeEventContent>::from_json(content),
                );
                self.room.client.send(r).await.map(|r| r.into())?
            }

            (Some(key), Some(delayed_event_parameters)) => {
                let r = delayed_events::delayed_state_event::unstable::Request::new_raw(
                    self.room.room_id().to_owned(),
                    key,
                    StateEventType::from(type_str),
                    delayed_event_parameters,
                    Raw::<AnyStateEventContent>::from_json(content),
                );
                self.room.client.send(r).await.map(|r| r.into())?
            }
        })
    }

    /// Send a request to the `/delayed_events`` endpoint ([MSC4140](https://github.com/matrix-org/matrix-spec-proposals/pull/4140))
    /// This can be used to refresh cancel or send a Delayed Event (An Event
    /// that is send ahead of time to the homeserver and gets distributed
    /// once it times out.)
    pub(crate) async fn update_delayed_event(
        &self,
        delay_id: String,
        action: UpdateAction,
    ) -> Result<delayed_events::update_delayed_event::unstable::Response> {
        let r = delayed_events::update_delayed_event::unstable::Request::new(delay_id, action);
        self.room.client.send(r).await.map_err(|error| Error::Http(Box::new(error)))
    }

    /// Starts forwarding new room events. Once the returned `EventReceiver`
    /// is dropped, forwarding will be stopped.
    pub(crate) fn events(&self) -> EventReceiver<Raw<AnyTimelineEvent>> {
        let (tx, rx) = unbounded_channel();
        let room_id = self.room.room_id().to_owned();

        let handle = self.room.add_event_handler(move |raw: Raw<AnySyncTimelineEvent>| {
            let _ = tx.send(attach_room_id(&raw, &room_id));
            async {}
        });
        let drop_guard = self.room.client().event_handler_drop_guard(handle);

        // The receiver will get a combination of state and message like events.
        // These always come from the timeline (rather than the state section of the
        // sync).
        EventReceiver { rx, _drop_guard: drop_guard }
    }

    /// Starts forwarding new updates to room state.
    pub(crate) fn state_updates(&self) -> StateUpdateReceiver {
        StateUpdateReceiver { room_updates: self.room.subscribe_to_updates() }
    }

    /// Starts forwarding new room events. Once the returned `EventReceiver`
    /// is dropped, forwarding will be stopped.
    pub(crate) fn to_device_events(&self) -> EventReceiver<Raw<AnyToDeviceEvent>> {
        let (tx, rx) = unbounded_channel();

        let room_id = self.room.room_id().to_owned();
        let to_device_handle = self.room.client().add_event_handler(

            async move |raw: Raw<AnyToDeviceEvent>, encryption_info: Option<EncryptionInfo>, client: Client| {

                // Some to-device traffic is used by the SDK for internal machinery.
                // They should not be exposed to widgets.
                if Self::should_filter_message_to_widget(&raw) {
                    return;
                }

                // Encryption can be enabled after the widget has been instantiated,
                // we want to keep track of the latest status
                let Some(room) = client.get_room(&room_id) else {
                    warn!("Room {room_id} not found in client.");
                    return;
                };

                let room_encrypted = room.latest_encryption_state().await
                    .map(|s| s.is_encrypted())
                    // Default consider encrypted
                    .unwrap_or(true);
                if room_encrypted {
                    // The room is encrypted so the to-device traffic should be too.
                    if encryption_info.is_none() {
                        warn!(
                            ?room_id,
                            "Received to-device event in clear for a widget in an e2e room, dropping."
                        );
                        return;
                    }

                    // There are no per-room specific decryption settings (trust requirements), so we can just send it to the
                    // widget.

                    // The raw to-device event contains more fields than the widget needs, so we need to clean it up
                    // to only type/content/sender.
                    #[derive(Deserialize, Serialize)]
                    struct CleanEventHelper<'a> {
                        #[serde(rename = "type")]
                        event_type: String,
                        #[serde(borrow)]
                        content: &'a RawJsonValue,
                        sender: String,
                    }

                    let _ = serde_json::from_str::<CleanEventHelper<'_>>(raw.json().get())
                        .and_then(|clean_event_helper| {
                            serde_json::value::to_raw_value(&clean_event_helper)
                        })
                        .map_err(|err| warn!(?room_id, "Unable to process to-device message for widget: {err}"))
                        .map(|box_value | {
                            tx.send(Raw::from_json(box_value))
                        });

                } else {
                    // forward to the widget
                    // It is ok to send an encrypted to-device message even if the room is clear.
                    let _ = tx.send(raw);
                }
            },
        );

        let drop_guard = self.room.client().event_handler_drop_guard(to_device_handle);
        EventReceiver { rx, _drop_guard: drop_guard }
    }

    fn should_filter_message_to_widget(raw_message: &Raw<AnyToDeviceEvent>) -> bool {
        let Ok(Some(event_type)) = raw_message.get_field::<String>("type") else {
            trace!("Invalid to-device message (no type) filtered out by widget driver.");
            return true;
        };

        // Filter out all the internal crypto related traffic.
        // The SDK has already zeroized the critical data, but let's not leak any
        // information
        let filtered = Self::is_internal_type(event_type.as_str());

        if filtered {
            trace!("To-device message of type <{event_type}> filtered out by widget driver.",);
        }
        filtered
    }

    fn is_internal_type(event_type: &str) -> bool {
        matches!(
            event_type,
            "m.dummy"
                | "m.room_key"
                | "m.room_key_request"
                | "m.forwarded_room_key"
                | "m.key.verification.request"
                | "m.key.verification.ready"
                | "m.key.verification.start"
                | "m.key.verification.cancel"
                | "m.key.verification.accept"
                | "m.key.verification.key"
                | "m.key.verification.mac"
                | "m.key.verification.done"
                | "m.secret.request"
                | "m.secret.send"
                // drop utd traffic
                | "m.room.encrypted"
        )
    }

    /// If the room the widget is in is encrypted, then the to-device message
    /// will be encrypted. If one of the named devices does not exist, then
    /// the call will fail with an error.
    pub(crate) async fn send_to_device(
        &self,
        event_type: ToDeviceEventType,
        messages: BTreeMap<
            OwnedUserId,
            BTreeMap<DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>>,
        >,
    ) -> Result<SendToDeviceEventResponse> {
        // TODO: block this at the negotiation stage, no reason to let widget believe
        // they can do that
        if Self::is_internal_type(&event_type.to_string()) {
            warn!("Widget tried to send internal to-device message <{}>, ignoring", event_type);
            // Silently return a success response, the widget will not receive the message
            return Ok(Default::default());
        }

        let client = self.room.client();

        let mut failures: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>> = BTreeMap::new();

        let room_encrypted = self
            .room
            .latest_encryption_state()
            .await
            .map(|s| s.is_encrypted())
            // Default consider encrypted
            .unwrap_or(true);

        if room_encrypted {
            trace!("Sending to-device message in encrypted room <{}>", self.room.room_id());

            // The widget-api uses a [user -> device -> content] map, but the
            // crypto-sdk API allow to encrypt a given content for multiple recipients.
            // Lets convert the [user -> device -> content] to a [content -> user -> device
            // map].
            let mut content_to_recipients_map: BTreeMap<
                &str,
                BTreeMap<OwnedUserId, Vec<DeviceIdOrAllDevices>>,
            > = BTreeMap::new();

            for (user_id, device_map) in messages.iter() {
                for (device_id, content) in device_map.iter() {
                    content_to_recipients_map
                        .entry(content.json().get())
                        .or_default()
                        .entry(user_id.clone())
                        .or_default()
                        .push(device_id.to_owned());
                }
            }

            // Encrypt and send this content
            for (content, user_to_list_of_device_id_or_all) in content_to_recipients_map {
                self.encrypt_and_send_content_to_devices_helper(
                    &event_type,
                    content,
                    user_to_list_of_device_id_or_all,
                    &mut failures,
                )
                .await?
            }

            let failures = failures
                .into_iter()
                .map(|(u, list_of_devices)| {
                    (u.into(), list_of_devices.into_iter().map(|d| d.into()).collect())
                })
                .collect();

            let response = SendToDeviceEventResponse { failures };
            Ok(response)
        } else {
            // send in clear
            let request = RumaToDeviceRequest::new_raw(event_type, TransactionId::new(), messages);
            client.send(request).await?;
            Ok(Default::default())
        }
    }

    // Helper to encrypt a single content to several devices
    async fn encrypt_and_send_content_to_devices_helper(
        &self,
        event_type: &ToDeviceEventType,
        content: &str,
        user_to_list_of_device_id_or_all: BTreeMap<OwnedUserId, Vec<DeviceIdOrAllDevices>>,
        failures: &mut BTreeMap<OwnedUserId, Vec<OwnedDeviceId>>,
    ) -> Result<()> {
        let client = self.room.client();
        let mut recipient_devices = Vec::<_>::new();

        for (user_id, recipient_device_ids) in user_to_list_of_device_id_or_all {
            let user_devices = client.encryption().get_user_devices(&user_id).await?;

            let user_devices = if recipient_device_ids.contains(&DeviceIdOrAllDevices::AllDevices) {
                // If the user wants to send to all devices, there's nothing to filter and no
                // need to inspect other entries in the user's device list.
                let devices: Vec<_> = user_devices.devices().collect();
                // TODO: What to do if the user has no devices?
                if devices.is_empty() {
                    warn!(
                        "Recipient list contains `AllDevices` but no devices found for user {user_id}."
                    )
                }
                // TODO: What if the `recipient_device_ids` has both
                // `AllDevices` and other devices but one of the  other devices is not found.
                if recipient_device_ids.len() > 1 {
                    warn!(
                        "The recipient_device_ids list for {user_id} contains both `AllDevices` and explicit `DeviceId` entries. Only consider `AllDevices`",
                    );
                }
                devices
            } else {
                // If the user wants to send to only some devices, filter out any devices that
                // aren't part of the recipient_device_ids list.
                let filtered_devices = user_devices
                    .devices()
                    .map(|device| (device.device_id().to_owned(), device))
                    .filter(|(device_id, _)| {
                        recipient_device_ids
                            .contains(&DeviceIdOrAllDevices::DeviceId(device_id.clone()))
                    });

                let (found_device_ids, devices): (BTreeSet<_>, Vec<_>) = filtered_devices.unzip();

                let list_of_devices: BTreeSet<_> = recipient_device_ids
                    .into_iter()
                    .filter_map(|d| as_variant!(d, DeviceIdOrAllDevices::DeviceId))
                    .collect();

                // Let's now find any devices that are part of the recipient_device_ids list but
                // were not found in our store.
                let missing_devices: Vec<_> =
                    list_of_devices.difference(&found_device_ids).map(|d| d.to_owned()).collect();
                if !missing_devices.is_empty() {
                    failures.insert(user_id, missing_devices);
                }
                devices
            };

            recipient_devices.extend(user_devices);
        }

        if !recipient_devices.is_empty() {
            // need to group by content
            let encrypt_and_send_failures = client
                .encryption()
                .encrypt_and_send_raw_to_device(
                    recipient_devices.iter().collect(),
                    &event_type.to_string(),
                    Raw::from_json_string(content.to_owned())?,
                    CollectStrategy::AllDevices,
                )
                .await?;

            for (user_id, device_id) in encrypt_and_send_failures {
                failures.entry(user_id).or_default().push(device_id)
            }
        }

        Ok(())
    }
}

/// A simple entity that wraps an `UnboundedReceiver`
/// along with the drop guard for the room event handler.
pub(crate) struct EventReceiver<E> {
    rx: UnboundedReceiver<E>,
    _drop_guard: EventHandlerDropGuard,
}

impl<T> EventReceiver<T> {
    pub(crate) async fn recv(&mut self) -> Option<T> {
        self.rx.recv().await
    }
}

/// A simple entity that wraps an `UnboundedReceiver` for the room state update
/// handler.
pub(crate) struct StateUpdateReceiver {
    room_updates: Receiver<RoomUpdate>,
}

impl StateUpdateReceiver {
    pub(crate) async fn recv(&mut self) -> Result<Vec<Raw<AnyStateEvent>>, RecvError> {
        loop {
            match self.room_updates.recv().await? {
                RoomUpdate::Joined { room, updates } => {
                    let state_events = match updates.state {
                        State::Before(events) => events,
                        State::After(events) => events,
                    };

                    if !state_events.is_empty() {
                        return Ok(state_events
                            .into_iter()
                            .map(|ev| attach_room_id_state(&ev, room.room_id()))
                            .collect());
                    }
                }
                _ => {
                    error!("MatrixDriver can only operate in joined rooms");
                    return Err(RecvError::Closed);
                }
            }
        }
    }
}

fn attach_room_id(raw_ev: &Raw<AnySyncTimelineEvent>, room_id: &RoomId) -> Raw<AnyTimelineEvent> {
    let mut ev_obj =
        raw_ev.deserialize_as_unchecked::<BTreeMap<String, Box<RawJsonValue>>>().unwrap();
    ev_obj.insert("room_id".to_owned(), serde_json::value::to_raw_value(room_id).unwrap());
    Raw::new(&ev_obj).unwrap().cast_unchecked()
}

fn attach_room_id_state(raw_ev: &Raw<AnySyncStateEvent>, room_id: &RoomId) -> Raw<AnyStateEvent> {
    attach_room_id(raw_ev.cast_ref(), room_id).cast_unchecked()
}

#[cfg(test)]
mod tests {
    use insta;
    use ruma::{events::AnyTimelineEvent, room_id, serde::Raw};
    use serde_json::{Value, json};

    use super::attach_room_id;

    #[test]
    fn test_add_room_id_to_raw() {
        let raw = Raw::new(&json!({
            "type": "m.room.message",
            "event_id": "$1676512345:example.org",
            "sender": "@user:example.org",
            "origin_server_ts": 1676512345,
            "content": {
                "msgtype": "m.text",
                "body": "Hello world"
            }
        }))
        .unwrap()
        .cast_unchecked();
        let room_id = room_id!("!my_id:example.org");
        let new = attach_room_id(&raw, room_id);

        insta::with_settings!({prepend_module_to_snapshot => false}, {
            insta::assert_json_snapshot!(new.deserialize_as::<Value>().unwrap())
        });

        let attached: AnyTimelineEvent = new.deserialize().unwrap();
        assert_eq!(attached.room_id(), room_id);
    }

    #[test]
    fn test_add_room_id_to_raw_override() {
        // What would happen if there is already a room_id in the raw content?
        // Ensure it is overridden with the given value
        let raw = Raw::new(&json!({
            "type": "m.room.message",
            "event_id": "$1676512345:example.org",
            "room_id": "!override_me:example.org",
            "sender": "@user:example.org",
            "origin_server_ts": 1676512345,
            "content": {
                "msgtype": "m.text",
                "body": "Hello world"
            }
        }))
        .unwrap()
        .cast_unchecked();
        let room_id = room_id!("!my_id:example.org");
        let new = attach_room_id(&raw, room_id);

        insta::with_settings!({prepend_module_to_snapshot => false}, {
            insta::assert_json_snapshot!(new.deserialize_as::<Value>().unwrap())
        });

        let attached: AnyTimelineEvent = new.deserialize().unwrap();
        assert_eq!(attached.room_id(), room_id);
    }
}
