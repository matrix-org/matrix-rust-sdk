#![cfg(test)]

use std::fs;
use std::panic;
use std::path::Path;

use crate::events::{
    collections::{
        all::{RoomEvent, StateEvent},
        only::Event,
    },
    presence::PresenceEvent,
    EventResult, TryFromRaw,
};
use crate::identifiers::{RoomId, UserId};
use crate::AsyncClient;

use mockito::{self, mock, Mock};

use crate::models::Room;

/// Easily create events to stream into either a Client or a `Room` for testing.
#[derive(Default)]
pub struct EventBuilder {
    /// The events that determine the state of a `Room`.
    room_events: Vec<RoomEvent>,
    /// The presence events that determine the presence state of a `RoomMember`.
    presence_events: Vec<PresenceEvent>,
    /// The state events that determine the state of a `Room`.
    state_events: Vec<StateEvent>,
    /// The ephemeral room events that determine the state of a `Room`.
    ephemeral: Vec<Event>,
    /// The account data events that determine the state of a `Room`.
    account_data: Vec<Event>,
}

pub struct RoomTestRunner {
    /// Used To test the models
    room: Option<Room>,
    /// The ephemeral room events that determine the state of a `Room`.
    ephemeral: Vec<Event>,
    /// The account data events that determine the state of a `Room`.
    account_data: Vec<Event>,
    /// The events that determine the state of a `Room`.
    room_events: Vec<RoomEvent>,
    /// The presence events that determine the presence state of a `RoomMember`.
    presence_events: Vec<PresenceEvent>,
    /// The state events that determine the state of a `Room`.
    state_events: Vec<StateEvent>,
}

pub struct ClientTestRunner<S, E> {
    /// Used when testing the whole client
    client: Option<AsyncClient<S, E>>,
    /// RoomId and UserId to use for the events.
    ///
    /// The RoomId must match the RoomId of the events to track.
    room_user_id: (RoomId, UserId),
    /// The ephemeral room events that determine the state of a `Room`.
    ephemeral: Vec<Event>,
    /// The account data events that determine the state of a `Room`.
    account_data: Vec<Event>,
    /// The events that determine the state of a `Room`.
    room_events: Vec<RoomEvent>,
    /// The presence events that determine the presence state of a `RoomMember`.
    presence_events: Vec<PresenceEvent>,
    /// The state events that determine the state of a `Room`.
    state_events: Vec<StateEvent>,
}

#[allow(dead_code)]
pub struct MockTestRunner<S, E> {
    /// Used when testing the whole client
    client: Option<AsyncClient<S, E>>,
    /// The ephemeral room events that determine the state of a `Room`.
    ephemeral: Vec<Event>,
    /// The account data events that determine the state of a `Room`.
    account_data: Vec<Event>,
    /// The events that determine the state of a `Room`.
    room_events: Vec<RoomEvent>,
    /// The presence events that determine the presence state of a `RoomMember`.
    presence_events: Vec<PresenceEvent>,
    /// The state events that determine the state of a `Room`.
    state_events: Vec<StateEvent>,
    /// `mokito::Mock`
    mock: Option<mockito::Mock>,
}

#[allow(dead_code)]
#[allow(unused_mut)]
impl EventBuilder {
    /// Add an event to the room events `Vec`.
    pub fn add_ephemeral_from_file<Ev: TryFromRaw, P: AsRef<Path>>(
        mut self,
        path: P,
        variant: fn(Ev) -> Event,
    ) -> Self {
        let val = fs::read_to_string(path.as_ref())
            .expect(&format!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventResult<Ev>>(&val)
            .unwrap()
            .into_result()
            .unwrap();
        self.ephemeral.push(variant(event));
        self
    }

    /// Add an event to the room events `Vec`.
    pub fn add_account_from_file<Ev: TryFromRaw, P: AsRef<Path>>(
        mut self,
        path: P,
        variant: fn(Ev) -> Event,
    ) -> Self {
        let val = fs::read_to_string(path.as_ref())
            .expect(&format!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventResult<Ev>>(&val)
            .unwrap()
            .into_result()
            .unwrap();
        self.account_data.push(variant(event));
        self
    }

    /// Add an event to the room events `Vec`.
    pub fn add_room_event_from_file<Ev: TryFromRaw, P: AsRef<Path>>(
        mut self,
        path: P,
        variant: fn(Ev) -> RoomEvent,
    ) -> Self {
        let val = fs::read_to_string(path.as_ref())
            .expect(&format!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventResult<Ev>>(&val)
            .unwrap()
            .into_result()
            .unwrap();
        self.room_events.push(variant(event));
        self
    }

    /// Add a state event to the state events `Vec`.
    pub fn add_state_event_from_file<Ev: TryFromRaw, P: AsRef<Path>>(
        mut self,
        path: P,
        variant: fn(Ev) -> StateEvent,
    ) -> Self {
        let val = fs::read_to_string(path.as_ref())
            .expect(&format!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventResult<Ev>>(&val)
            .unwrap()
            .into_result()
            .unwrap();
        self.state_events.push(variant(event));
        self
    }

    /// Add a presence event to the presence events `Vec`.
    pub fn add_presence_event_from_file<P: AsRef<Path>>(mut self, path: P) -> Self {
        let val = fs::read_to_string(path.as_ref())
            .expect(&format!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventResult<PresenceEvent>>(&val)
            .unwrap()
            .into_result()
            .unwrap();
        self.presence_events.push(event);
        self
    }

    /// Consumes `ResponseBuilder and returns a `TestRunner`.
    ///
    /// The `TestRunner` streams the events to the client and holds methods to make assertions
    /// about the state of the client.
    pub fn build_mock_runner<S, E, P: Into<mockito::Matcher>>(
        mut self,
        method: &str,
        path: P,
    ) -> MockTestRunner<S, E> {
        let body = serde_json::json! {
            {
                "device_one_time_keys_count": {},
                "next_batch": "s526_47314_0_7_1_1_1_11444_1",
                "device_lists": {
                    "changed": [],
                    "left": []
                },
                "rooms": {
                    "invite": {},
                    "join": {
                        "!SVkFJHzfwvuaIEawgC:localhost": {
                            "summary": {},
                            "account_data": {
                                "events": self.account_data
                            },
                            "ephemeral": {
                                "events": self.ephemeral
                            },
                            "state": {
                                "events": self.state_events
                            },
                            "timeline": {
                                "events": self.room_events,
                                "limited": true,
                                "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                            },
                            "unread_notifications": {
                                "highlight_count": 0,
                                "notification_count": 11
                            }
                        }
                    },
                    "leave": {}
                },
                "to_device": {
                    "events": []
                },
                "presence": {
                    "events": []
                }
            }
        };
        let mock = Some(
            mock(method, path)
                .with_status(200)
                .with_body(body.to_string())
                .create(),
        );
        MockTestRunner {
            client: None,
            ephemeral: Vec::new(),
            account_data: Vec::new(),
            room_events: Vec::new(),
            presence_events: Vec::new(),
            state_events: Vec::new(),
            mock,
        }
    }

    /// Consumes `ResponseBuilder and returns a `TestRunner`.
    ///
    /// The `TestRunner` streams the events to the `AsyncClient` and holds methods to make assertions
    /// about the state of the `AsyncClient`.
    pub fn build_client_runner<S, E>(
        self,
        room_id: RoomId,
        user_id: UserId,
    ) -> ClientTestRunner<S, E> {
        ClientTestRunner {
            client: None,
            room_user_id: (room_id, user_id),
            ephemeral: self.ephemeral,
            account_data: self.account_data,
            room_events: self.room_events,
            presence_events: self.presence_events,
            state_events: self.state_events,
        }
    }

    /// Consumes `ResponseBuilder and returns a `TestRunner`.
    ///
    /// The `TestRunner` streams the events to the `Room` and holds methods to make assertions
    /// about the state of the `Room`.
    pub fn build_room_runner(self, room_id: &RoomId, user_id: &UserId) -> RoomTestRunner {
        RoomTestRunner {
            room: Some(Room::new(room_id, user_id)),
            ephemeral: self.ephemeral,
            account_data: self.account_data,
            room_events: self.room_events,
            presence_events: self.presence_events,
            state_events: self.state_events,
        }
    }
}

impl RoomTestRunner {
    /// Set `Room`
    pub fn set_room(&mut self, room: Room) -> &mut Self {
        self.room = Some(room);
        self
    }

    fn stream_room_events(&mut self) {
        let room = self
            .room
            .as_mut()
            .expect("`Room` must be set use `RoomTestRunner::set_room`");
        for event in &self.account_data {
            match event {
                // Event::IgnoredUserList(iu) => room.handle_ignored_users(iu),
                Event::Presence(p) => room.receive_presence_event(p),
                // Event::PushRules(pr) => room.handle_push_rules(pr),
                _ => todo!("implement more account data events"),
            };
        }

        for event in &self.ephemeral {
            match event {
                // Event::IgnoredUserList(iu) => room.handle_ignored_users(iu),
                Event::Presence(p) => room.receive_presence_event(p),
                // Event::PushRules(pr) => room.handle_push_rules(pr),
                _ => todo!("implement more account data events"),
            };
        }

        for event in &self.room_events {
            room.receive_timeline_event(event);
        }
        for event in &self.presence_events {
            room.receive_presence_event(event);
        }
        for event in &self.state_events {
            room.receive_state_event(event);
        }
    }

    pub fn to_room(&mut self) -> &mut Room {
        self.stream_room_events();
        self.room.as_mut().unwrap()
    }
}

impl<S, E> ClientTestRunner<S, E> {
    pub fn set_client(&mut self, client: AsyncClient<S, E>) -> &mut Self {
        self.client = Some(client);
        self
    }

    async fn stream_client_events(&mut self) {
        let mut cli = self
            .client
            .as_ref()
            .expect("`AsyncClient` must be set use `ClientTestRunner::set_client`")
            .base_client
            .write()
            .await;

        let room_id = &self.room_user_id.0;

        for event in &self.account_data {
            match event {
                // Event::IgnoredUserList(iu) => room.handle_ignored_users(iu),
                Event::Presence(p) => cli.receive_presence_event(room_id, p).await,
                // Event::PushRules(pr) => room.handle_push_rules(pr),
                _ => todo!("implement more account data events"),
            };
        }

        for event in &self.ephemeral {
            cli.receive_ephemeral_event(room_id, event).await;
        }

        for event in &self.room_events {
            cli.receive_joined_timeline_event(room_id, &mut EventResult::Ok(event.clone()))
                .await;
        }
        for event in &self.presence_events {
            cli.receive_presence_event(room_id, event).await;
        }
        for event in &self.state_events {
            cli.receive_joined_state_event(room_id, event).await;
        }
    }

    pub async fn to_client(&mut self) -> &mut AsyncClient<S, E> {
        self.stream_client_events().await;
        self.client.as_mut().unwrap()
    }
}

impl<S, E> MockTestRunner<S, E> {
    pub fn set_client(&mut self, client: AsyncClient<S, E>) -> &mut Self {
        self.client = Some(client);
        self
    }

    pub fn set_mock(mut self, mock: Mock) -> Self {
        self.mock = Some(mock);
        self
    }

    pub async fn to_client(&mut self) -> Result<&mut AsyncClient<S, E>, crate::Error> {
        self.client
            .as_mut()
            .unwrap()
            .sync(crate::SyncSettings::default())
            .await?;

        Ok(self.client.as_mut().unwrap())
    }
}
