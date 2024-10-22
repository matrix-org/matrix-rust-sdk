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
        AnyMessageLikeEventContent, AnyStateEventContent, AnySyncTimelineEvent, AnyTimelineEvent,
        AnyToDeviceEvent, AnyToDeviceEventContent, MessageLikeEventType, StateEventType,
        TimelineEventType, ToDeviceEventType,
    },
    serde::Raw,
    to_device::DeviceIdOrAllDevices,
    OwnedUserId, RoomId, TransactionId, UserId,
};
use serde_json::{json, value::RawValue as RawJsonValue, Value};
use tokio::{
    spawn,
    sync::mpsc::{unbounded_channel, UnboundedReceiver},
};
use tracing::{error, info};

use super::{machine::SendEventResponse, StateKeySelector};
use crate::{
    encryption::identities::Device, event_handler::EventHandlerDropGuard, room::MessagesOptions,
    Client, Error, HttpResult, Result, Room,
};

/// Thin wrapper around a [`Room`] that provides functionality relevant for
/// widgets.
pub(crate) struct MatrixDriver {
    room: Room,
}

// pub enum SendToDeviceResult {
//     HttpError(HttpResult<send_event_to_device::v3::Response>),
// }
// impl From<HttpResult<send_event_to_device::v3::Response>> for SendEventResponse {
//     fn from(value: HttpResult<send_event_to_device::v3::Response>) -> Self {
//         SendToDeviceResult::HttpError(value)
//     }
// }
impl MatrixDriver {
    /// Creates a new `MatrixDriver` for a given `room`.
    pub(crate) fn new(room: Room) -> Self {
        Self { room }
    }

    /// Requests an OpenID token for the current user.
    pub(crate) async fn get_open_id(&self) -> HttpResult<OpenIdResponse> {
        let user_id = self.room.own_user_id().to_owned();
        self.room.client.send(OpenIdRequest::new(user_id), None).await
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
        Ok(messages.chunk.into_iter().map(|ev| ev.event.cast()).collect())
    }

    pub(crate) async fn read_state_events(
        &self,
        event_type: StateEventType,
        state_key: &StateKeySelector,
    ) -> Result<Vec<Raw<AnyTimelineEvent>>> {
        let room_id = self.room.room_id();
        let convert = |sync_or_stripped_state| match sync_or_stripped_state {
            RawAnySyncOrStrippedState::Sync(ev) => Some(attach_room_id(ev.cast_ref(), room_id)),
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

    /// Sends a given `event` to the room.
    pub(crate) async fn send(
        &self,
        event_type: TimelineEventType,
        state_key: Option<String>,
        content: Box<RawJsonValue>,
        delayed_event_parameters: Option<delayed_events::DelayParameters>,
    ) -> Result<SendEventResponse> {
        let type_str = event_type.to_string();
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
                self.room.client.send(r, None).await.map(|r| r.into())?
            }
            (Some(key), Some(delayed_event_parameters)) => {
                let r = delayed_events::delayed_state_event::unstable::Request::new_raw(
                    self.room.room_id().to_owned(),
                    key,
                    StateEventType::from(type_str),
                    delayed_event_parameters,
                    Raw::<AnyStateEventContent>::from_json(content),
                );
                self.room.client.send(r, None).await.map(|r| r.into())?
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
    ) -> HttpResult<delayed_events::update_delayed_event::unstable::Response> {
        let r = delayed_events::update_delayed_event::unstable::Request::new(delay_id, action);
        self.room.client.send(r, None).await
    }

    /// Starts forwarding new room events. Once the returned `EventReceiver`
    /// is dropped, forwarding will be stopped.
    pub(crate) fn events(&self) -> EventReceiver<AnyTimelineEvent> {
        let (tx, rx) = unbounded_channel();
        let room_id = self.room.room_id().to_owned();
        let handle = self.room.add_event_handler(move |raw: Raw<AnySyncTimelineEvent>| {
            let _ = tx.send(attach_room_id(&raw, &room_id));
            async {}
        });

        let drop_guard = self.room.client().event_handler_drop_guard(handle);
        EventReceiver { rx, _drop_guard: drop_guard }
    }

    /// Starts forwarding new room events. Once the returned `EventReceiver`
    /// is dropped, forwarding will be stopped.
    pub(crate) fn to_device_events(&self) -> EventReceiver<AnyToDeviceEvent> {
        let (tx, rx) = unbounded_channel();

        let to_device_handle = self.room.client().add_event_handler(
            move |raw: Raw<AnyToDeviceEvent>, encryption_info: Option<EncryptionInfo>| {
                // Deserialize the Raw<AnyToDeviceEvent> to a mutable structure
                let mut event_with_encryption_flag: Value =
                    raw.deserialize_as().expect("Invalid event JSON");

                if let Some(content) = event_with_encryption_flag.get_mut("content") {
                    content["encrypted"] = json!(encryption_info.is_some());
                }
                let ev_for_widget = match Raw::<AnyToDeviceEvent>::from_json_string(
                    event_with_encryption_flag.to_string(),
                ) {
                    Ok(ev) => ev,
                    Err(_) => raw,
                };

                let _ = tx.send(ev_for_widget);
                async {}
            },
        );

        let drop_guard = self.room.client().event_handler_drop_guard(to_device_handle);
        EventReceiver { rx, _drop_guard: drop_guard }
    }

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
        // This encrypts the content for a device.
        // A device_id can be "*" in this case the function also computes all devices for a user and
        // returns an iterator of devices.
        async fn encrypted_content_for_device_or_all_devices(
            client: &Client,
            unencrypted: &Raw<AnyToDeviceEventContent>,
            device_or_all_id: DeviceIdOrAllDevices,
            user_id: &UserId,
            event_type: &ToDeviceEventType,
        ) -> Result<impl Iterator<Item = (DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>)>>
        {
            let user_devices = client.encryption().get_user_devices(&user_id).await?;

            let devices: Vec<Device> = match device_or_all_id {
                DeviceIdOrAllDevices::DeviceId(device_id) => {
                    vec![user_devices.get(&device_id)].into_iter().flatten().collect()
                }
                DeviceIdOrAllDevices::AllDevices => user_devices.devices().collect(),
            };

            let content: Value = unencrypted.deserialize_as().map_err(Into::<Error>::into)?;
            let event_type = event_type.clone();
            let device_content_tasks = devices.into_iter().map(|device| spawn({
            let value = event_type.clone();
            let content = content.clone();
            async move {
                let a =match device
                    .inner
                    .encrypt_event_raw(&value.to_string(), &content)
                    .await{
                        Ok(encrypted) => Some((device.device_id().to_owned().into(), encrypted.cast())),
                        Err(e) =>{ info!("Failed to encrypt to_device event from widget for device: {} because, {}", device.device_id(), e); None},
                    };
                    a
            }
            }));
            let t = join_all(device_content_tasks).await.into_iter().flatten().flatten();
            Ok(t)
        }

        // Here we convert the device content map for one user into the same content map with encrypted content
        // This needs to flatten the vectors we get from `encrypted_content_for_device_or_all_devices`
        // since one DeviceIdOrAllDevices id can be multiple devices.
        async fn encrypted_device_content_map(
            client: &Client,
            user_id: &UserId,
            event_type: &ToDeviceEventType,
            device_content_map: BTreeMap<DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>>,
        ) -> BTreeMap<DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>> {
            let device_map_futures =
                device_content_map.into_iter().map(|(device_or_all_id, content)| spawn({let client = client.clone();let user_id = user_id.to_owned();let event_type = event_type.clone();async move {
                    encrypted_content_for_device_or_all_devices(
                        &client,
                        &content,
                        device_or_all_id,
                        &user_id,
                        &event_type,
                    )
                    .await
                    .map_err(|e| info!("could not encrypt content for to device widget event content: {}. because, {}", content.json(), e))
                    .ok()
                }}));
            // The first flatten takes the iterator of Option<Device>'s iterators and converts it to just a iterator over Option<(Device, data)>'s.
            // The second flatten takes the the iterator over Option<(Device,data)>'s and converts it to just a iterator over Device
            join_all(device_map_futures).await.into_iter().flatten().flatten().flatten().collect()
        }

        // We first want to get all missing session before we start any to device sending!
        client.claim_one_time_keys(messages.iter().map(|(u, _)| u.as_ref())).await?;

        let request = match encrypted {
            true => {
                let encrypted_content: BTreeMap<
                    OwnedUserId,
                    BTreeMap<DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>>,
                > = join_all(messages.into_iter().map(|(user_id, device_content_map)| {
                    let event_type = event_type.clone();
                    async move {
                        (
                            user_id.clone(),
                            encrypted_device_content_map(
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
                    event_type.clone(),
                    TransactionId::new(),
                    encrypted_content,
                )
            }
            false => RumaToDeviceRequest::new_raw(event_type, TransactionId::new(), messages),
        };

        let response = client.send(request, None).await;
        // if let Ok(res){
        //     client.mark_request_as_sent(request.request_id(), &res).await;
        // };
        response.map_err(Into::into)
    }
}

/// A simple entity that wraps an `UnboundedReceiver`
/// along with the drop guard for the room event handler.
pub(crate) struct EventReceiver<E> {
    rx: UnboundedReceiver<Raw<E>>,
    _drop_guard: EventHandlerDropGuard,
}

impl<T> EventReceiver<T> {
    pub(crate) async fn recv(&mut self) -> Option<Raw<T>> {
        self.rx.recv().await
    }
}

fn attach_room_id(raw_ev: &Raw<AnySyncTimelineEvent>, room_id: &RoomId) -> Raw<AnyTimelineEvent> {
    let mut ev_obj = raw_ev.deserialize_as::<BTreeMap<String, Box<RawJsonValue>>>().unwrap();
    ev_obj.insert("room_id".to_owned(), serde_json::value::to_raw_value(room_id).unwrap());
    Raw::new(&ev_obj).unwrap().cast()
}
