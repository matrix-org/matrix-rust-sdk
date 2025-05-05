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
        AnyMessageLikeEventContent, AnyStateEvent, AnyStateEventContent, AnySyncStateEvent,
        AnySyncTimelineEvent, AnyTimelineEvent, AnyToDeviceEvent, AnyToDeviceEventContent,
        MessageLikeEventType, StateEventType, TimelineEventType, ToDeviceEventType,
    },
    serde::{from_raw_json_value, Raw},
    to_device::DeviceIdOrAllDevices,
    EventId, OwnedUserId, RoomId, TransactionId,
};
use serde_json::{value::RawValue as RawJsonValue, Value};
use tokio::sync::{
    broadcast::{error::RecvError, Receiver},
    mpsc::{unbounded_channel, UnboundedReceiver},
};
use tracing::error;

use super::{machine::SendEventResponse, StateKeySelector};
use crate::{
    event_handler::EventHandlerDropGuard, room::MessagesOptions, sync::RoomUpdate, Error, Result,
    Room,
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
            .map(|ev| ev.into_raw().cast())
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

        let handle = self.room.add_event_handler(move |raw: Raw<AnySyncTimelineEvent>| {
            let _ = tx.send(attach_room_id(raw.cast_ref(), &room_id));
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

        let to_device_handle = self.room.client().add_event_handler(
            // TODO: encryption support for to-device is not yet supported. Needs an Olm
            // EncryptionInfo. The widgetAPI expects a boolean `encrypted` to be added
            // (!) to the raw content to know if the to-device message was encrypted or
            // not (as per MSC3819).
            move |raw: Raw<AnyToDeviceEvent>, _: Option<EncryptionInfo>| {
                let _ = tx.send(raw);
                async {}
            },
        );

        let drop_guard = self.room.client().event_handler_drop_guard(to_device_handle);
        EventReceiver { rx, _drop_guard: drop_guard }
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
            return Err(Error::UnknownError(
                "Sending encrypted to-device events is not supported by the widget driver.".into(),
            ));
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
                    if !updates.state.is_empty() {
                        return Ok(updates
                            .state
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
    let mut ev_obj = raw_ev.deserialize_as::<BTreeMap<String, Box<RawJsonValue>>>().unwrap();
    ev_obj.insert("room_id".to_owned(), serde_json::value::to_raw_value(room_id).unwrap());
    Raw::new(&ev_obj).unwrap().cast()
}

fn attach_room_id_state(raw_ev: &Raw<AnySyncStateEvent>, room_id: &RoomId) -> Raw<AnyStateEvent> {
    attach_room_id(raw_ev.cast_ref(), room_id).cast()
}

#[cfg(test)]
mod tests {
    use insta;
    use ruma::{events::AnyTimelineEvent, room_id, serde::Raw};
    use serde_json::{json, Value};

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
        .cast();
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
        .cast();
        let room_id = room_id!("!my_id:example.org");
        let new = attach_room_id(&raw, room_id);

        insta::with_settings!({prepend_module_to_snapshot => false}, {
            insta::assert_json_snapshot!(new.deserialize_as::<Value>().unwrap())
        });

        let attached: AnyTimelineEvent = new.deserialize().unwrap();
        assert_eq!(attached.room_id(), room_id);
    }
}
