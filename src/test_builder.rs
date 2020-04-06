


#[cfg(test)]
mod test_it {
    #![allow(unused)]

    use std::fs;
    use std::path::Path;
    use std::panic;
    use std::convert::TryFrom;
    use std::str::FromStr;
    use std::time::Duration;
    
    use crate::identifiers::{RoomId, UserId};
    use crate::events::{
        collections::all::RoomEvent,
        room::{
            power_levels::PowerLevelsEvent,
        },
        EventResult, FromRaw, TryFromRaw,
    };
    use crate::{AsyncClient, Session, SyncSettings};

    use ansi_term::Colour;
    use serde_json::Value;
    use mockito::{mock, Matcher};
    use serde::{de::DeserializeOwned, Deserialize, Serialize};
    use url::Url;

    use crate::models::Room;

    #[derive(Default)]
    pub struct ResponseBuilder {
        events: Vec<RoomEvent>,
    }

    pub struct TestRunner {
        /// Used when testing the whole client
        client: Option<AsyncClient>,
        /// Used To test the models
        room: Option<Room>,
        /// When testing the models a vec of RoomEvents is needed.
        events: Vec<RoomEvent>,
        /// A `Vec` of callbacks that should assert something about the client.
        /// 
        /// The callback should panic if the state is unexpected (use `assert_*!` macro)
        client_assertions: Vec<fn(&AsyncClient)>,
        /// A `Vec` of callbacks that should assert something about the room.
        /// 
        /// The callback should panic if the state is unexpected (use `assert_*!` macro)
        room_assertions: Vec<fn(&Room)>,
        /// `mokito::Mock`
        mock: Option<mockito::Mock>,
    }

    impl ResponseBuilder {

        /// Creates an `IncomingResponse` to hold events for a sync.
        pub fn create_sync_response(mut self) -> Self {

            self
        }

        /// Just throw events at the client, not part of a specific response.
        pub fn create_event_stream(mut self) -> Self {

            self
        }

        /// Add an event to the events `Vec`.
        pub fn add_event_from_file<Ev: TryFromRaw, P: AsRef<Path>>(mut self, path: P, variant: fn(Ev) -> RoomEvent) -> Self {
            let val = fs::read_to_string(path.as_ref()).expect(&format!("file not found {:?}", path.as_ref()));
            let event = serde_json::from_str::<ruma_events::EventResult<Ev>>(&val).unwrap().into_result().unwrap();
            self.add_event(variant(event));
            self
        }

        fn add_event(&mut self, event: RoomEvent) {
            self.events.push(event)
        }

        /// Consumes `ResponseBuilder and returns a `TestRunner`.
        /// 
        /// The `TestRunner` streams the events to the client and enables methods to set assertions
        /// about the state of the client. 
        pub fn build_responses(mut self, method: &str, path: &str) -> TestRunner {
            let body = serde_json::to_string(&self.events).unwrap();
            let mock = Some(mock(method, path)
                .with_status(200)
                .with_body(body)
                .create());
            
            TestRunner {
                client: None,
                room: None,
                events: Vec::new(),
                client_assertions: Vec::new(),
                room_assertions: Vec::new(),
                mock,
            }
        }

        /// Consumes `ResponseBuilder and returns a `TestRunner`.
        /// 
        /// The `TestRunner` streams the events to the client and enables methods to set assertions
        /// about the state of the client. 
        pub fn build_room_events(mut self, room_id: &RoomId, user_id: &UserId) -> TestRunner {
            TestRunner {
                client: None,
                room: Some(Room::new(room_id, user_id)),
                events: self.events,
                client_assertions: Vec::new(),
                room_assertions: Vec::new(),
                mock: None,
            }
        }
    }

    impl TestRunner {
        pub fn set_client(mut self, client: AsyncClient) -> Self {
            self.client = Some(client);
            self
        }

        pub fn add_client_assert(mut self, assert: fn(&AsyncClient)) -> Self {
            self.client_assertions.push(assert);
            self
        }

        pub fn add_room_assert(mut self, assert: fn(&Room)) -> Self {
            self.room_assertions.push(assert);
            self
        }

        fn run_client_tests(mut self) -> Result<(), Vec<String>> {
            Ok(())
        }

        fn run_room_tests(mut self) -> Result<(), Vec<String>> {
            let mut errs = Vec::new();
            let mut room = self.room.unwrap();
            for event in &self.events {
                match event {
                    RoomEvent::RoomMember(m) => room.handle_membership(m),
                    RoomEvent::RoomName(n) => room.handle_room_name(n),
                    RoomEvent::RoomCanonicalAlias(ca) => room.handle_canonical(ca),
                    RoomEvent::RoomAliases(a) => room.handle_room_aliases(a),
                    RoomEvent::RoomPowerLevels(p) => room.handle_power_level(p),
                    // RoomEvent::RoomEncryption(e) => room.handle_encryption_event(e),
                    _ => todo!("implement more RoomEvent variants"),
                };
            }
            for assert in self.room_assertions {
                if let Err(e) = panic::catch_unwind(|| assert(&room)) {
                    errs.push(stringify!(e).to_string());
                }
            }
            if errs.is_empty() {
                Ok(())
            } else {
                Err(errs)
            }
        }

        pub fn run_test(mut self) {
            let errs = if let Some(room) = &self.room {
                self.run_room_tests()
            } else if let Some(cli) = &self.client {
                self.run_client_tests()
            } else {
                panic!("must have either AsyncClient or Room")
            };

            if let Err(errs) = errs {
                let err_str = errs.join(&format!("{}\n", Colour::Red.paint("Error: ")));
                if !errs.is_empty() {
                    panic!("{}", Colour::Red.paint("some tests failed"));
                }
            }
        }
    }

    fn test_room_users(room: &Room) {
        assert_eq!(room.members.len(), 1);
    }

    fn test_room_power(room: &Room) {
        assert!(room.power_levels.is_some());
        assert_eq!(room.power_levels.as_ref().unwrap().kick, js_int::Int::new(50).unwrap());
        let admin = room.members.get(&UserId::try_from("@example:localhost").unwrap()).unwrap();
        assert_eq!(admin.power_level.unwrap(), js_int::Int::new(100).unwrap());
        println!("{:#?}", room);
    }

    #[test]
    fn room_events() {
        let rid = RoomId::try_from("!roomid:room.com").unwrap();
        let uid = UserId::try_from("@example:localhost").unwrap();
        let mut bld = ResponseBuilder::default();
        let runner = bld.add_event_from_file("./tests/data/events/member.json", RoomEvent::RoomMember)
            .add_event_from_file("./tests/data/events/power_levels.json", RoomEvent::RoomPowerLevels)
            .build_room_events(&rid, &uid)
            .add_room_assert(test_room_power)
            .add_room_assert(test_room_users);
        
        runner.run_test();
    }

}
