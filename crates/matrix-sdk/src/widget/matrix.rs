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

use std::collections::BTreeMap;

use futures_util::future::join_all;
use matrix_sdk_base::deserialized_responses::{EncryptionInfo, RawAnySyncOrStrippedState};
use ruma::{
    api::client::{
        account::request_openid_token::v3::{Request as OpenIdRequest, Response as OpenIdResponse},
        delayed_events::{self, update_delayed_event::unstable::UpdateAction},
        filter::RoomEventFilter,
        to_device::send_event_to_device::{self, v3::Request as RumaToDeviceRequest},
    },
    assign,
    events::{
        AnyMessageLikeEventContent, AnyStateEventContent, AnySyncMessageLikeEvent,
        AnySyncStateEvent, AnyTimelineEvent, AnyToDeviceEvent, AnyToDeviceEventContent,
        MessageLikeEventType, StateEventType, TimelineEventType, ToDeviceEventType,
    },
    serde::{from_raw_json_value, Raw},
    to_device::DeviceIdOrAllDevices,
    EventId, OwnedRoomId, OwnedUserId, TransactionId,
};
use serde::{Deserialize, Serialize};
use serde_json::{value::RawValue as RawJsonValue, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tracing::{error, info};

use super::{machine::SendEventResponse, StateKeySelector};
use crate::{event_handler::EventHandlerDropGuard, room::MessagesOptions, Error, Result, Room};

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

    /// Reads the latest `limit` events of a given `event_type` from the room.
    pub(crate) async fn read_message_like_events(
        &self,
        event_type: MessageLikeEventType,
        limit: u32,
    ) -> Result<Vec<Raw<AnyTimelineEvent>>> {
        let options = assign!(MessagesOptions::backward(), {
            limit: limit.into(),
            filter: assign!(RoomEventFilter::default(), {
                types: Some(vec![event_type.to_string()])
            }),
        });

        let messages = self.room.messages(options).await?;
        Ok(messages.chunk.into_iter().map(|ev| ev.into_raw().cast()).collect())
    }

    pub(crate) async fn read_state_events(
        &self,
        event_type: StateEventType,
        state_key: &StateKeySelector,
    ) -> Result<Vec<Raw<AnyTimelineEvent>>> {
        let room_id = self.room.room_id();
        let convert = |sync_or_stripped_state| match sync_or_stripped_state {
            RawAnySyncOrStrippedState::Sync(ev) => {
                add_props_to_raw(&ev, Some(room_id.to_owned()), None)
                    .map(Raw::cast)
                    .map_err(|e| {
                        error!("failed to convert event from `get_state_event` response:{}", e)
                    })
                    .ok()
            }
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
                self.room.send_raw(&type_str, content).await?.event_id,
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

        // Get only message like events from the timeline section of the sync.
        let _tx = tx.clone();
        let _room_id = room_id.clone();
        let handle_msg_like =
            self.room.add_event_handler(move |raw: Raw<AnySyncMessageLikeEvent>| {
                match add_props_to_raw(&raw, Some(_room_id), None) {
                    Ok(event_with_room_id) => {
                        let _ = _tx.send(event_with_room_id.cast());
                    }
                    Err(e) => {
                        error!("Failed to attach room id to message like event: {}", e);
                    }
                }
                async {}
            });
        let drop_guard_msg_like = self.room.client().event_handler_drop_guard(handle_msg_like);
        let _room_id = room_id;
        let _tx = tx;
        // Get only all state events from the state section of the sync.
        let handle_state = self.room.add_event_handler(move |raw: Raw<AnySyncStateEvent>| {
            match add_props_to_raw(&raw, Some(_room_id.to_owned()), None) {
                Ok(event_with_room_id) => {
                    let _ = _tx.send(event_with_room_id.cast());
                }
                Err(e) => {
                    error!("Failed to attach room id to state event: {}", e);
                }
            }
            async {}
        });
        let drop_guard_state = self.room.client().event_handler_drop_guard(handle_state);

        // The receiver will get a combination of state and message like events.
        // The state events will come from the state section of the sync (to always
        // represent current resolved state). All state events in the timeline
        // section of the sync will not be forwarded to the widget.
        // TODO annotate the events and send both timeline and state section state
        // events.
        EventReceiver { rx, _drop_guards: vec![drop_guard_msg_like, drop_guard_state] }
    }

    /// Starts forwarding new room events. Once the returned `EventReceiver`
    /// is dropped, forwarding will be stopped.
    pub(crate) fn to_device_events(&self) -> EventReceiver<Raw<AnyToDeviceEvent>> {
        let (tx, rx) = unbounded_channel();

        let to_device_handle = self.room.client().add_event_handler(
            move |raw: Raw<AnyToDeviceEvent>, encryption_info: Option<EncryptionInfo>| {
                match add_props_to_raw(&raw, None, encryption_info.as_ref()) {
                    Ok(ev) => {
                        let _ = tx.send(ev);
                    }
                    Err(e) => {
                        error!("Failed to attach encryption flag to to_device event: {}", e);
                    }
                }
                async {}
            },
        );

        let drop_guard = self.room.client().event_handler_drop_guard(to_device_handle);
        EventReceiver { rx, _drop_guards: vec![drop_guard] }
    }

    /// It will ignore all devices where errors occurred or where the device is
    /// not verified or where th user has a has_verification_violation.
    pub(crate) async fn send_to_device(
        &self,
        event_type: ToDeviceEventType,
        encrypted: bool,
        messages: BTreeMap<
            OwnedUserId,
            BTreeMap<DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>>,
        >,
    ) -> Result<send_event_to_device::v3::Response> {
        let client = self.room.client();

        let request = if encrypted {
            // We first want to get all missing session before we start any to device
            // sending!
            client.claim_one_time_keys(messages.keys().map(|u| u.as_ref())).await?;
            let encrypted_content: BTreeMap<
                OwnedUserId,
                BTreeMap<DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>>,
            > = join_all(messages.into_iter().map(|(user_id, device_content_map)| {
                let event_type = event_type.clone();
                async move {
                    (
                        user_id.clone(),
                        to_device_crypto::encrypted_device_content_map(
                            &self.room.client(),
                            &user_id,
                            &event_type,
                            device_content_map,
                        )
                        .await,
                    )
                }
            }))
            .await
            .into_iter()
            .collect();

            RumaToDeviceRequest::new_raw(
                ToDeviceEventType::RoomEncrypted,
                TransactionId::new(),
                encrypted_content,
            )
        } else {
            RumaToDeviceRequest::new_raw(event_type, TransactionId::new(), messages)
        };

        let response = client.send(request).await;

        response.map_err(Into::into)
    }
}

/// A simple entity that wraps an `UnboundedReceiver`
/// along with the drop guard for the room event handler.
pub(crate) struct EventReceiver<E> {
    rx: UnboundedReceiver<E>,
    _drop_guards: Vec<EventHandlerDropGuard>,
}

impl<T> EventReceiver<T> {
    pub(crate) async fn recv(&mut self) -> Option<T> {
        self.rx.recv().await
    }
}

// `room_id` and `encryption` is the only modification we need to do to the
// events otherwise they are just forwarded raw to the widget.
// This is why we do not serialization the whole event but pass it as a raw
// value through the widget driver and only serialize here to allow potimizing
// with `serde(borrow)`.
#[derive(Deserialize, Serialize)]
struct RoomIdEncryptionSerializer {
    #[serde(skip_serializing_if = "Option::is_none")]
    room_id: Option<OwnedRoomId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypted: Option<bool>,
    #[serde(flatten)]
    rest: Value,
}

/// Attach additional properties to the event.
///
/// Attach a room id to the event. This is needed because the widget API
/// requires the room id to be present in the event.
///
/// Attach the `ecryption` flag to the event. This is needed so the widget gets
/// informed if an event is encrypted or not. Since the client is responsible
/// for decrypting the event, there otherwise is no way for the widget to know
/// if its an encrypted (signed/trusted) event or not.
fn add_props_to_raw<T>(
    raw: &Raw<T>,
    room_id: Option<OwnedRoomId>,
    encryption_info: Option<&EncryptionInfo>,
) -> Result<Raw<T>> {
    match raw.deserialize_as::<RoomIdEncryptionSerializer>() {
        Ok(mut event) => {
            event.room_id = room_id.or(event.room_id);
            event.encrypted = encryption_info.map(|_| true).or(event.encrypted);
            info!("rest is: {:?}", event.rest);
            Ok(Raw::new(&event)?.cast())
        }
        Err(e) => Err(Error::from(e)),
    }
}

/// Move this into the `matrix_crypto` crate!
/// This module contains helper functions to encrypt to device events.
mod to_device_crypto {
    use std::collections::BTreeMap;

    use futures_util::future::join_all;
    use ruma::{
        events::{AnyToDeviceEventContent, ToDeviceEventType},
        serde::Raw,
        to_device::DeviceIdOrAllDevices,
        UserId,
    };
    use serde_json::Value;
    use tracing::{info, warn};

    use crate::{encryption::identities::Device, executor::spawn, Client, Error, Result};

    /// This encrypts to device content for a collection of devices.
    /// It will ignore all devices where errors occurred or where the device
    /// is not verified or where th user has a has_verification_violation.
    async fn encrypted_content_for_devices(
        unencrypted_content: &Raw<AnyToDeviceEventContent>,
        devices: Vec<Device>,
        event_type: &ToDeviceEventType,
    ) -> Result<impl Iterator<Item = (DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>)>> {
        let content: Value = unencrypted_content.deserialize_as().map_err(Into::<Error>::into)?;
        let event_type = event_type.clone();
        let device_content_tasks = devices.into_iter().map(|device| spawn({
                let event_type = event_type.clone();
                let content = content.clone();

                async move {
                    // This is not yet used. It is incompatible with the spa guest mode (the spa will not verify its crypto identity)
                    // if !device.is_cross_signed_by_owner() {
                    //     info!("Device {} is not verified, skipping encryption", device.device_id());
                    //     return None;
                    // }
                    match device
                        .inner
                        .encrypt_event_raw(&event_type.to_string(), &content)
                        .await {
                            Ok(encrypted) => Some((device.device_id().to_owned().into(), encrypted.cast())),
                            Err(e) =>{ info!("Failed to encrypt to_device event from widget for device: {} because, {}", device.device_id(), e); None},
                        }
                }
            }));
        let device_encrypted_content_map =
            join_all(device_content_tasks).await.into_iter().flatten().flatten();
        Ok(device_encrypted_content_map)
    }

    /// Convert the device content map for one user into the same content
    /// map with encrypted content This needs to flatten the vectors
    /// we get from `encrypted_content_for_devices`
    /// since one `DeviceIdOrAllDevices` id can be multiple devices.
    pub(super) async fn encrypted_device_content_map(
        client: &Client,
        user_id: &UserId,
        event_type: &ToDeviceEventType,
        device_content_map: BTreeMap<DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>>,
    ) -> BTreeMap<DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>> {
        let device_map_futures =
                device_content_map.into_iter().map(|(device_or_all_id, content)| spawn({
                    let client = client.clone();
                    let user_id = user_id.to_owned();
                    let event_type = event_type.clone();
                    async move {
                        let Ok(user_devices) = client.encryption().get_user_devices(&user_id).await else {
                            warn!("Failed to get user devices for user: {}", user_id);
                            return None;
                        };
                        // This is not yet used. It is incompatible with the spa guest mode (the spa will not verify its crypto identity)
                        // let Ok(user_identity) = client.encryption().get_user_identity(&user_id).await else{
                        //     warn!("Failed to get user identity for user: {}", user_id);
                        //     return None;
                        // };
                        // if user_identity.map(|i|i.has_verification_violation()).unwrap_or(false) {
                        //     info!("User {} has a verification violation, skipping encryption", user_id);
                        //     return None;
                        // }
                        let devices: Vec<Device> = match device_or_all_id {
                            DeviceIdOrAllDevices::DeviceId(device_id) => {
                                vec![user_devices.get(&device_id)].into_iter().flatten().collect()
                            }
                            DeviceIdOrAllDevices::AllDevices => user_devices.devices().collect(),
                        };
                        encrypted_content_for_devices(
                            &content,
                            devices,
                            &event_type,
                        )
                        .await
                        .map_err(|e| info!("WidgetDriver: could not encrypt content for to device widget event content: {}. because, {}", content.json(), e))
                        .ok()
                }}));
        let content_map_iterator = join_all(device_map_futures).await.into_iter();

        // The first flatten takes the iterator over Result<Option<impl Iterator<Item =
        // (DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>)>>, JoinError>>
        // and flattens the Result (drops Err() items)
        // The second takes the iterator over: Option<impl Iterator<Item =
        // (DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>)>>
        // and flattens the Option (drops None items)
        // The third takes the iterator over iterators: impl Iterator<Item =
        // (DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>)>
        // and flattens it to just an iterator over (DeviceIdOrAllDevices,
        // Raw<AnyToDeviceEventContent>)
        content_map_iterator.flatten().flatten().flatten().collect()
    }
}

#[cfg(test)]
mod tests {
    use ruma::{room_id, serde::Raw};
    use serde_json::json;

    use super::add_props_to_raw;

    #[test]
    fn test_app_props_to_raw() {
        let raw = Raw::new(&json!({
            "encrypted": true,
            "type": "m.room.message",
            "content": {
                "body": "Hello world"
            }
        }))
        .unwrap();
        let room_id = room_id!("!my_id:example.org");
        let new = add_props_to_raw(&raw, Some(room_id.to_owned()), None).unwrap();
        assert_eq!(
            serde_json::to_value(new).unwrap(),
            json!({
                "encrypted": true,
                "room_id": "!my_id:example.org",
                "type": "m.room.message",
                "content": {
                    "body": "Hello world"
                }
            })
        );
    }
}
