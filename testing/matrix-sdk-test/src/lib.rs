use std::fmt;

use http::Response;
pub use matrix_sdk_test_macros::async_test;
use once_cell::sync::Lazy;
use ruma::{
    RoomId, UserId,
    api::{
        IncomingResponse, OutgoingResponse, client::sync::sync_events::v3::Response as SyncResponse,
    },
    room_id, user_id,
};
use serde_json::Value as JsonValue;

/// Create a `Raw<AnyMessageLikeEventContent>` from arbitrary JSON.
///
/// Forwards all arguments to [`serde_json::json`].
#[macro_export]
macro_rules! message_like_event_content {
    ($( $tt:tt )*) => {
        ::ruma::serde::Raw::new(&::serde_json::json!( $($tt)* ))
            .unwrap()
            .cast_unchecked::<::ruma::events::AnyMessageLikeEventContent>()
    }
}

/// Create a `Raw<AnyTimelineEvent>` from arbitrary JSON.
///
/// Forwards all arguments to [`serde_json::json`].
#[macro_export]
macro_rules! timeline_event {
    ($( $tt:tt )*) => {
        ::ruma::serde::Raw::new(&::serde_json::json!( $($tt)* ))
            .unwrap()
            .cast_unchecked::<::ruma::events::AnyTimelineEvent>()
    }
}

/// Create a `Raw<AnySyncTimelineEvent>` from arbitrary JSON.
///
/// Forwards all arguments to [`serde_json::json`].
#[macro_export]
macro_rules! sync_timeline_event {
    ($( $tt:tt )*) => {
        ::ruma::serde::Raw::new(&::serde_json::json!( $($tt)* ))
            .unwrap()
            .cast_unchecked::<::ruma::events::AnySyncTimelineEvent>()
    }
}

/// Create a `Raw<AnySyncStateEvent>` from arbitrary JSON.
///
/// Forwards all arguments to [`serde_json::json`].
#[macro_export]
macro_rules! sync_state_event {
    ($( $tt:tt )*) => {
        ::ruma::serde::Raw::new(&::serde_json::json!( $($tt)* ))
            .unwrap()
            .cast_unchecked::<::ruma::events::AnySyncStateEvent>()
    }
}

/// Create a `Raw<AnyStrippedStateEvent>` from arbitrary JSON.
///
/// Forwards all arguments to [`serde_json::json`].
#[macro_export]
macro_rules! stripped_state_event {
    ($( $tt:tt )*) => {
        ::ruma::serde::Raw::new(&::serde_json::json!( $($tt)* ))
            .unwrap()
            .cast_unchecked::<::ruma::events::AnyStrippedStateEvent>()
    }
}

#[cfg(not(target_family = "wasm"))]
pub mod mocks;

pub mod event_factory;
pub mod notification_settings;
mod sync_builder;
pub mod test_json;

pub use self::sync_builder::{
    InvitedRoomBuilder, JoinedRoomBuilder, KnockedRoomBuilder, LeftRoomBuilder, PresenceTestEvent,
    RoomAccountDataTestEvent, StateTestEvent, StrippedStateTestEvent, SyncResponseBuilder,
    bulk_room_members,
};

pub static ALICE: Lazy<&UserId> = Lazy::new(|| user_id!("@alice:server.name"));
pub static BOB: Lazy<&UserId> = Lazy::new(|| user_id!("@bob:other.server"));
pub static CAROL: Lazy<&UserId> = Lazy::new(|| user_id!("@carol:other.server"));

/// The default room ID for tests.
pub static DEFAULT_TEST_ROOM_ID: Lazy<&RoomId> =
    Lazy::new(|| room_id!("!SVkFJHzfwvuaIEawgC:localhost"));

/// Embedded sync response files
pub enum SyncResponseFile {
    All,
    Default,
    DefaultWithSummary,
    Invite,
    Leave,
    Voip,
}

/// Get specific API responses for testing
pub fn sync_response(kind: SyncResponseFile) -> SyncResponse {
    let data: &JsonValue = match kind {
        SyncResponseFile::All => &test_json::MORE_SYNC,
        SyncResponseFile::Default => &test_json::SYNC,
        SyncResponseFile::DefaultWithSummary => &test_json::DEFAULT_SYNC_SUMMARY,
        SyncResponseFile::Invite => &test_json::INVITE_SYNC,
        SyncResponseFile::Leave => &test_json::LEAVE_SYNC,
        SyncResponseFile::Voip => &test_json::VOIP_SYNC,
    };

    ruma_response_from_json(data)
}

/// Build a typed Ruma [`IncomingResponse`] object from a json body.
pub fn ruma_response_from_json<ResponseType: IncomingResponse>(
    json: &serde_json::Value,
) -> ResponseType {
    let json_bytes = serde_json::to_vec(json).expect("JSON-serialization of response value failed");
    let http_response =
        Response::builder().status(200).body(json_bytes).expect("Failed to build HTTP response");
    ResponseType::try_from_http_response(http_response).expect("Can't parse the response json")
}

/// Serialise a typed Ruma [`OutgoingResponse`] object to JSON.
pub fn ruma_response_to_json<ResponseType: OutgoingResponse>(
    response: ResponseType,
) -> serde_json::Value {
    let http_response: Response<Vec<u8>> =
        response.try_into_http_response().expect("Failed to build HTTP response");
    let json_bytes = http_response.into_body();
    serde_json::from_slice(&json_bytes).expect("Can't parse the response JSON")
}

#[derive(Debug)] // required to be able to return TestResult from #[test] fns
pub enum TestError {}

// If this was just `T: Debug`, it would conflict with
// the `impl From<T> for T` in `std`.
//
// Adding a dummy `Display` bound works around this.
impl<T: fmt::Display + fmt::Debug> From<T> for TestError {
    #[track_caller]
    fn from(value: T) -> Self {
        panic!("error: {value:?}")
    }
}

pub type TestResult = Result<(), TestError>;
