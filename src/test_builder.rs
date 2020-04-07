#![cfg(test)]

use std::fs;
use std::path::Path;
use std::panic;

use crate::identifiers::{RoomId, UserId};
use crate::events::{
    collections::all::{Event, RoomEvent, StateEvent},
    presence::PresenceEvent,
    EventResult, TryFromRaw,
};
use crate::{AsyncClient};

use ansi_term::Colour;
use mockito::mock;

use crate::models::Room;

/// `assert` to use in `TestRunner`.
/// 
/// This returns an `Err` on failure, instead of panicking.
#[macro_export]
macro_rules! assert_ {
    ($truth:expr) => {
        if !$truth {
            return Err(format!(r#"assertion failed: `(left == right)`
    expression: `{:?}`
    failed at {}"#, 
                stringify!($truth),
                file!(),
            ))
        }
    };
}

/// `assert_eq` to use in `TestRunner.
/// 
/// This returns an `Err` on failure, instead of panicking.
#[macro_export]
macro_rules! assert_eq_ {
    ($left:expr, $right:expr) => ({
        match (&$left, &$right) {
            (left_val, right_val) => {
                if !(*left_val == *right_val) {
                    return Err(format!(r#"assertion failed: `(left == right)`
    left: `{:?}`,
    right: `{:?}`
    failed at {}:{}"#,
                        &*left_val,
                        &*right_val,
                        file!(),
                        line!()
                    ))
                }
            }
        }
    });
    ($left:expr, $right:expr,) => ({
        $crate::assert_eq!($left, $right)
    });
    ($left:expr, $right:expr, $($arg:tt)+) => ({
        match (&($left), &($right)) {
            (left_val, right_val) => {
                if !(*left_val == *right_val) {
                    return Err(format!(r#"assertion failed: `(left == right)`
    left: `{:?}`,
    right: `{:?}` : {}
    failed at {}:{}"#,
                        &*left_val,
                        &*right_val,
                        $crate::format_args!($($arg)+),
                        file!(),
                        line!(),
                    ))
                }
            }
        }
    });
}

/// `assert_ne` to use in `TestRunner.
/// 
/// This returns an `Err` on failure, instead of panicking.
#[macro_export]
macro_rules! assert_ne_ {
    ($left:expr, $right:expr) => ({
        match (&$left, &$right) {
            (left_val, right_val) => {
                if (*left_val == *right_val) {
                    return Err(format!(r#"assertion failed: `(left == right)`
    left: `{:?}`,
    right: `{:?}`
    failed at {}:{}"#,
                        &*left_val,
                        &*right_val,
                        file!(),
                        line!()
                    ))
                }
            }
        }
    });
    ($left:expr, $right:expr,) => ({
        $crate::assert_eq!($left, $right)
    });
    ($left:expr, $right:expr, $($arg:tt)+) => ({
        match (&($left), &($right)) {
            (left_val, right_val) => {
                if (*left_val == *right_val) {
                    return Err(format!(r#"assertion failed: `(left == right)`
    left: `{:?}`,
    right: `{:?}` : {}
    failed at {}:{}"#,
                        &*left_val,
                        &*right_val,
                        $crate::format_args!($($arg)+),
                        file!(),
                        line!(),
                    ))
                }
            }
        }
    });
}

/// Convenience macro for declaring an `async` assert function to store in the `TestRunner`.
///
/// Declares an async function that can be stored in a struct.
/// 
/// # Examples
/// ```rust
/// # use matrix_sdk::AsyncClient;
/// # use url::Url; 
/// async_assert!{ 
///     async fn foo(cli: &AsyncClient) -> Result<(), String> {
///         assert_eq_!(cli.homeserver(), Url::new("matrix.org"))
///         Ok(())
///     }
/// }
/// ```
#[macro_export]
macro_rules! async_assert {
    (
        $( #[$attr:meta] )*
        $pub:vis async fn $fname:ident<$lt:lifetime> ( $($args:tt)* ) $(-> $Ret:ty)? {
            $($body:tt)*
        }
    ) => (
        $( #[$attr] )*
        #[allow(unused_parens)]
        $pub fn $fname<$lt> ( $($args)* )
            -> ::std::pin::Pin<::std::boxed::Box<dyn ::std::future::Future<Output = ($($Ret)?)>
        + ::std::marker::Send + $lt>>
        {
            ::std::boxed::Box::pin(async move { $($body)* })
        }
    );
    (
        $( #[$attr:meta] )*
        $pub:vis async fn $fname:ident ( $($args:tt)* ) $(-> $Ret:ty)? {
            $($body:tt)*
        }
    ) => (
        $( #[$attr] )*
        #[allow(unused_parens)]
        $pub fn $fname ( $($args)* )
            -> ::std::pin::Pin<::std::boxed::Box<(dyn ::std::future::Future<Output = ($($Ret)?)>
        + ::std::marker::Send)>>
        {
            ::std::boxed::Box::pin(async move { $($body)* })
        }
    )
}

type DynFuture<'lt, T> = ::std::pin::Pin<Box<dyn 'lt + Send + ::std::future::Future<Output = T>>>;
pub type AsyncAssert = fn(&AsyncClient) -> DynFuture<Result<(), String>>;

#[derive(Default)]
pub struct EventBuilder {
    /// The events that determine the state of a `Room`.
    ///
    /// When testing the models `RoomEvent`s are needed.
    room_events: Vec<RoomEvent>,
    /// The presence events that determine the presence state of a `RoomMember`.
    presence_events: Vec<PresenceEvent>,
    /// The state events that determine the state of a `Room`.
    state_events: Vec<StateEvent>,
    /// The state events that determine the state of a `Room`.
    non_room_events: Vec<Event>,
}

#[allow(dead_code)]
pub struct TestRunner {
    /// Used when testing the whole client
    client: Option<AsyncClient>,
    /// Used To test the models
    room: Option<Room>,
    /// The non room events that determine the state of a `Room`.
    ///
    /// These are ephemeral and account events.
    non_room_events: Vec<Event>,
    /// The events that determine the state of a `Room`.
    ///
    /// When testing the models `RoomEvent`s are needed.
    room_events: Vec<RoomEvent>,
    /// The presence events that determine the presence state of a `RoomMember`.
    presence_events: Vec<PresenceEvent>,
    /// The state events that determine the state of a `Room`.
    state_events: Vec<StateEvent>,
    /// A `Vec` of callbacks that should assert something about the client.
    /// 
    /// The callback should use the provided `assert_`, `assert_*_` macros.
    client_assertions: Vec<AsyncAssert>,
    /// A `Vec` of callbacks that should assert something about the room.
    /// 
    /// The callback should use the provided `assert_`, `assert_*_` macros.
    room_assertions: Vec<fn(&Room) -> Result<(), String>>,
    /// `mokito::Mock`
    mock: Option<mockito::Mock>,
}

#[allow(dead_code)]
#[allow(unused_mut)]
impl EventBuilder {

    /// Creates an `IncomingResponse` to hold events for a sync.
    pub fn create_sync_response(mut self) -> Self {

        self
    }

    /// Just throw events at the client, not part of a specific response.
    pub fn create_event_stream(mut self) -> Self {

        self
    }

    /// Add an event to the room events `Vec`.
    pub fn add_non_event_from_file<Ev: TryFromRaw, P: AsRef<Path>>(mut self, path: P, variant: fn(Ev) -> Event) -> Self {
        let val = fs::read_to_string(path.as_ref()).expect(&format!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventResult<Ev>>(&val).unwrap().into_result().unwrap();
        self.non_room_events.push(variant(event));
        self
    }

    /// Add an event to the room events `Vec`.
    pub fn add_room_event_from_file<Ev: TryFromRaw, P: AsRef<Path>>(mut self, path: P, variant: fn(Ev) -> RoomEvent) -> Self {
        let val = fs::read_to_string(path.as_ref()).expect(&format!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventResult<Ev>>(&val).unwrap().into_result().unwrap();
        self.room_events.push(variant(event));
        self
    }

    /// Add a state event to the state events `Vec`.
    pub fn add_state_event_from_file<Ev: TryFromRaw, P: AsRef<Path>>(mut self, path: P, variant: fn(Ev) -> StateEvent) -> Self {
        let val = fs::read_to_string(path.as_ref()).expect(&format!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventResult<Ev>>(&val).unwrap().into_result().unwrap();
        self.state_events.push(variant(event));
        self
    }

    /// Add a presence event to the presence events `Vec`.
    pub fn add_presence_event_from_file<P: AsRef<Path>>(mut self, path: P) -> Self {
        let val = fs::read_to_string(path.as_ref()).expect(&format!("file not found {:?}", path.as_ref()));
        let event = serde_json::from_str::<EventResult<PresenceEvent>>(&val).unwrap().into_result().unwrap();
        self.presence_events.push(event);
        self
    }

    /// Consumes `ResponseBuilder and returns a `TestRunner`.
    /// 
    /// The `TestRunner` streams the events to the client and holds methods to make assertions
    /// about the state of the client.
    pub fn build_client_runner(mut self, method: &str, path: &str) -> TestRunner {
        // TODO serialize this properly
        let body = serde_json::to_string(&self.room_events).unwrap();
        let mock = Some(mock(method, path)
            .with_status(200)
            .with_body(body)
            .create());
        
        TestRunner {
            client: None,
            room: None,
            non_room_events: Vec::new(),
            room_events: Vec::new(),
            presence_events: Vec::new(),
            state_events: Vec::new(),
            client_assertions: Vec::new(),
            room_assertions: Vec::new(),
            mock,
        }
    }

    /// Consumes `ResponseBuilder and returns a `TestRunner`.
    /// 
    /// The `TestRunner` streams the events to the `Room` and holds methods to make assertions
    /// about the state of the `Room`. 
    pub fn build_room_runner(self, room_id: &RoomId, user_id: &UserId) -> TestRunner {
        TestRunner {
            client: None,
            room: Some(Room::new(room_id, user_id)),
            non_room_events: self.non_room_events,
            room_events: self.room_events,
            presence_events: self.presence_events,
            state_events: self.state_events,
            client_assertions: Vec::new(),
            room_assertions: Vec::new(),
            mock: None,
        }
    }
}

#[allow(dead_code)]
impl TestRunner {
    pub fn set_client(mut self, client: AsyncClient) -> Self {
        self.client = Some(client);
        self
    }

    /// Set `Room`
    pub fn set_room(mut self, room: Room) -> Self {
        self.room = Some(room);
        self
    }

    pub fn add_client_assert(mut self, assert: AsyncAssert) -> Self {
        self.client_assertions.push(assert);
        self
    }

    pub fn add_room_assert(mut self, assert: fn(&Room) -> Result<(), String>) -> Self {
        self.room_assertions.push(assert);
        self
    }

    async fn run_client_tests(&mut self) -> Result<(), Vec<String>> {
        let mut errs = Vec::new();
        let mut cli = self.client.as_ref().unwrap().base_client.write().await;
        let room_id = &self.room.as_ref().unwrap().room_id;

        for event in &self.non_room_events {
            match event {
                // Event::IgnoredUserList(iu) => room.handle_ignored_users(iu),
                Event::Presence(p) => cli.receive_presence_event(room_id, p).await,
                // Event::PushRules(pr) => room.handle_push_rules(pr),
                // TODO receive ephemeral events
                _ => todo!("implement more non room events"),
            };
        }

        for event in &self.room_events {
            cli.receive_joined_timeline_event(room_id, &mut EventResult::Ok(event.clone())).await;
        }
        for event in &self.presence_events {
            cli.receive_presence_event(room_id, event).await;
        }
        for event in &self.state_events {
            cli.receive_joined_state_event(room_id, event).await;
        }

        for assert in &mut self.client_assertions {
            if let Err(e) = assert(self.client.as_ref().unwrap()).await {
                errs.push(e);
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    fn run_room_tests(&mut self) -> Result<(), Vec<String>> {
        let mut errs = Vec::new();
        let room = self.room.as_mut().unwrap();

        for event in &self.non_room_events {
            match event {
                // Event::IgnoredUserList(iu) => room.handle_ignored_users(iu),
                Event::Presence(p) => room.receive_presence_event(p),
                // Event::PushRules(pr) => room.handle_push_rules(pr),
                // TODO receive ephemeral events
                _ => todo!("implement more non room events"),
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

        for assert in &mut self.room_assertions {
            if let Err(e) = assert(&room) {
                errs.push(e);
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    pub async fn run_test(mut self) {
        let (count, errs) = if let Some(_) = &self.room {
            (self.room_assertions.len(), self.run_room_tests())
        } else if let Some(_) = &self.client {
            (self.client_assertions.len(), self.run_client_tests().await)
        } else {
            panic!("must have either AsyncClient or Room")
        };

        if let Err(errs) = errs {
            let err_str = errs.join(&format!("\n\n"));
            println!("{}\n{}", Colour::Red.paint("Error: "), err_str);
            if !errs.is_empty() {
                panic!("{} tests failed", errs.len());
            } else {
                eprintln!("{}. {} passed", Colour::Green.paint("Ok"), count);
            }
        }
    }
}
