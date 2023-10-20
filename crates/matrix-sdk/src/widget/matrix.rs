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

use matrix_sdk_base::deserialized_responses::RawAnySyncOrStrippedState;
use ruma::{
    api::client::{
        account::request_openid_token::v3::{Request as OpenIdRequest, Response as OpenIdResponse},
        filter::RoomEventFilter,
    },
    assign,
    events::{
        AnySyncTimelineEvent, AnyTimelineEvent, MessageLikeEventType, StateEventType,
        TimelineEventType,
    },
    serde::Raw,
    OwnedEventId, RoomId,
};
use serde_json::{value::RawValue as RawJsonValue, Value as JsonValue};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tracing::error;

use super::StateKeySelector;
use crate::{
    event_handler::EventHandlerDropGuard, room::MessagesOptions, HttpResult, Result, Room,
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
        content: JsonValue,
    ) -> Result<OwnedEventId> {
        let type_str = event_type.to_string();
        Ok(match state_key {
            Some(key) => self.room.send_state_event_raw(content, &type_str, &key).await?.event_id,
            None => self.room.send_raw(content, &type_str, None).await?.event_id,
        })
    }

    /// Starts forwarding new room events. Once the returned `EventReceiver`
    /// is dropped, forwarding will be stopped.
    pub(crate) fn events(&self) -> EventReceiver {
        let (tx, rx) = unbounded_channel();
        let room_id = self.room.room_id().to_owned();
        let handle = self.room.add_event_handler(move |raw: Raw<AnySyncTimelineEvent>| {
            let _ = tx.send(attach_room_id(&raw, &room_id));
            async {}
        });

        let drop_guard = self.room.client().event_handler_drop_guard(handle);
        EventReceiver { rx, _drop_guard: drop_guard }
    }
}

/// A simple entity that wraps an `UnboundedReceiver`
/// along with the drop guard for the room event handler.
pub(crate) struct EventReceiver {
    rx: UnboundedReceiver<Raw<AnyTimelineEvent>>,
    _drop_guard: EventHandlerDropGuard,
}

impl EventReceiver {
    pub(crate) async fn recv(&mut self) -> Option<Raw<AnyTimelineEvent>> {
        self.rx.recv().await
    }
}

fn attach_room_id(raw_ev: &Raw<AnySyncTimelineEvent>, room_id: &RoomId) -> Raw<AnyTimelineEvent> {
    let mut ev_obj = raw_ev.deserialize_as::<BTreeMap<String, Box<RawJsonValue>>>().unwrap();
    ev_obj.insert("room_id".to_owned(), serde_json::value::to_raw_value(room_id).unwrap());
    Raw::new(&ev_obj).unwrap().cast()
}
