


#[cfg(test)]
mod test_it {
    #![allow(unused)]

    use std::fs;
    use std::path::Path;
    
    use crate::identifiers::UserId;
    use crate::events::collections::all::RoomEvent;
    use crate::{AsyncClient, Session, SyncSettings};

    use serde_json as json;
    use mockito::{mock, Matcher};
    use serde::{Serialize, Deserialize};
    use url::Url;

    use std::convert::TryFrom;
    use std::str::FromStr;
    use std::time::Duration;
    use std::panic;


    pub struct ResponseBuilder {
        events: Vec<RoomEvent>
    }

    pub struct TestRunner {
        client: Option<AsyncClient>,
        /// A `Vec` of callbacks that should assert something about the client.
        /// 
        /// The callback should panic if the state is unexpected (use `assert_*!` macro)
        assertions: Vec<fn(&AsyncClient)>,
        /// `mokito::Mock`
        mock: mockito::Mock,
    }

    impl ResponseBuilder {

        /// Creates an `IncomingResponse` to hold events for a sync.
        pub fn create_sync_response(&mut self) -> &mut Self {

            self
        }

        /// Just throw events at the client, not part of a specific response.
        pub fn create_event_stream(&mut self) -> &mut Self {

            self
        }

        /// Add an event either to the event stream or the appropriate `IncomingResponse` field.
        pub fn add_event_from_file<S: Serialize, P: AsRef<Path>>(&mut self, path: P) -> &mut Self {
            let json = fs::read_to_string(path.as_ref()).expect(&format!("file not found {:?}", path.as_ref()));
            let event = serde_json::from_str::<S>(&json).expect("deserialization failed");
            self.events.push(event);
            self
        }

        fn add_event<T, U>(&mut self, content: T, event: fn(T) -> RoomEvent) {
            self.events.push(event(content))
        }

        /// Consumes `ResponseBuilder and returns a `TestRunner`.
        /// 
        /// The `TestRunner` streams the events to the client and enables methods to set assertions
        /// about the state of the client. 
        pub fn build(self) -> TestRunner {
            let mock = mock("POST", "/_matrix/client/r0/login")
                .with_status(200)
                .with_body_from_file("tests/data/login_response.json")
                .create();
            
            TestRunner {
                client: None,
                assertions: Vec::new(),
                mock,
            }
        }
    }

    impl TestRunner {
        pub fn set_client(&mut self, client: AsyncClient) -> &mut Self {
            self.client = Some(client);
            self
        }
        pub fn run_test(self) -> Result<(), Vec<String>> {


            Ok(())
        }
    }

}
