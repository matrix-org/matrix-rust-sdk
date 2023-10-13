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

use matrix_sdk_base::deserialized_responses::RawAnySyncOrStrippedState;
use ruma::{
    api::client::{
        account::request_openid_token::v3::{Request as OpenIdRequest, Response as OpenIdResponse},
        filter::RoomEventFilter,
    },
    assign,
    events::{AnySyncTimelineEvent, AnyTimelineEvent, TimelineEventType},
    serde::Raw,
    OwnedEventId, RoomId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use crate::{
    event_handler::EventHandlerDropGuard, room::MessagesOptions, HttpResult, Result, Room,
};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum StateKeySelector {
    Key(String),
    Any(bool),
}

/// Thin wrapper around the Matrix API that provides convenient high level
/// functions.
pub struct MatrixDriver {
    room: Room,
}

impl MatrixDriver {
    /// Creates a new `MatrixDriver` for a given `room`.
    pub fn new(room: Room) -> Self {
        Self { room }
    }

    /// Requests an OpenID token for the current user.
    pub async fn get_open_id(&self) -> HttpResult<OpenIdResponse> {
        let user_id = self.room.own_user_id().to_owned();
        self.room.client.send(OpenIdRequest::new(user_id), None).await
    }

    /// Reads the latest `limit` events of a given `event_type` from the room.
    pub async fn read(
        &self,
        event_type: TimelineEventType,
        limit: Option<u32>,
        state_key: Option<StateKeySelector>,
    ) -> Result<Vec<Raw<AnyTimelineEvent>>> {
        let room_id = self.room.room_id();

        let convert_and_limit =
            |events: Vec<RawAnySyncOrStrippedState>| -> Vec<Raw<AnyTimelineEvent>> {
                let event_iter = events.into_iter().filter_map(|ev| match ev {
                    RawAnySyncOrStrippedState::Sync(raw) => {
                        Some(raw.cast::<AnySyncTimelineEvent>().with_room_id(room_id))
                    }
                    RawAnySyncOrStrippedState::Stripped(_) => None,
                });
                if let Some(limit) = limit.map(|l| l.try_into().unwrap_or(usize::MAX)) {
                    return event_iter.take(limit).collect();
                }
                event_iter.collect()
            };

        match state_key {
            Some(state_key) => match state_key {
                StateKeySelector::Any(_) => {
                    let events = self.room.get_state_events(event_type.to_string().into()).await;
                    return events.map(|events| convert_and_limit(events));
                }
                StateKeySelector::Key(state_key) => {
                    let events = self
                        .room
                        .get_state_events_for_keys(
                            event_type.to_string().into(),
                            &[state_key.as_str()],
                        )
                        .await;
                    return events.map(|events| convert_and_limit(events));
                }
            },
            None => {
                let options = assign!(MessagesOptions::backward(), {
                    limit: limit.unwrap_or(50).into(),
                    filter: assign!(RoomEventFilter::default(), {
                        types: Some(vec![event_type.to_string()])
                    })
                });
                // get message like events
                let messages = self.room.messages(options).await?;

                // Filter the timeline events.
                let events = messages
                    .chunk
                    .into_iter()
                    .map(|ev| ev.event.cast())
                    // TODO: Log events that failed to decrypt?
                    // TODO: add filter
                    .collect();
                Ok(events)
            }
        }
    }

    /// Sends a given `event` to the room.
    pub async fn send(
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
    pub fn events(&self) -> EventReceiver {
        let (tx, rx) = unbounded_channel();
        let room_id = self.room.room_id().to_owned();
        let handle = self.room.add_event_handler(move |raw: Raw<AnySyncTimelineEvent>| {
            let _ = tx.send(raw.with_room_id(&room_id));
            async {}
        });

        let drop_guard = self.room.client().event_handler_drop_guard(handle);
        EventReceiver { rx, _drop_guard: drop_guard }
    }
}

/// A simple entity that wraps an `UnboundedReceiver`
/// along with the drop guard for the room event handler.
pub struct EventReceiver {
    rx: UnboundedReceiver<Raw<AnyTimelineEvent>>,
    _drop_guard: EventHandlerDropGuard,
}

impl EventReceiver {
    pub async fn recv(&mut self) -> Option<Raw<AnyTimelineEvent>> {
        self.rx.recv().await
    }
}

trait AddRoomId {
    fn with_room_id(&self, room_id: &RoomId) -> Raw<AnyTimelineEvent>;
}
impl AddRoomId for Raw<AnySyncTimelineEvent> {
    fn with_room_id(&self, room_id: &RoomId) -> Raw<AnyTimelineEvent> {
        // deserialize should be possible if Raw<AnySyncTimelineEvent> is possible
        let mut ev_value = self.deserialize_as::<serde_json::Value>().unwrap();
        let ev_obj = ev_value.as_object_mut().unwrap();
        ev_obj.insert("room_id".to_owned(), room_id.as_str().into());
        let ev_with_room_id = serde_json::from_value::<Raw<AnyTimelineEvent>>(ev_value).unwrap();
        ev_with_room_id
    }
}
