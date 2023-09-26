use http::Response;
pub use matrix_sdk_test_macros::async_test;
use once_cell::sync::Lazy;
use ruma::{
    api::{client::sync::sync_events::v3::Response as SyncResponse, IncomingResponse},
    user_id, UserId,
};
use serde_json::Value as JsonValue;

/// Create a `Raw<AnyTimelineEvent>` from arbitrary JSON.
///
/// Forwards all arguments to [`serde_json::json`].
#[macro_export]
macro_rules! timeline_event {
    ($( $tt:tt )*) => {
        ::ruma::serde::Raw::new(&::serde_json::json!( $($tt)* ))
            .unwrap()
            .cast::<::ruma::events::AnyTimelineEvent>()
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
            .cast::<::ruma::events::AnySyncTimelineEvent>()
    }
}

mod event_builder;
pub mod notification_settings;
mod sync_builder;
pub mod test_json;

pub use self::{
    event_builder::EventBuilder,
    sync_builder::{
        bulk_room_members, EphemeralTestEvent, GlobalAccountDataTestEvent, InvitedRoomBuilder,
        JoinedRoomBuilder, LeftRoomBuilder, PresenceTestEvent, RoomAccountDataTestEvent,
        StateTestEvent, StrippedStateTestEvent, SyncResponseBuilder,
    },
};

pub static ALICE: Lazy<&UserId> = Lazy::new(|| user_id!("@alice:server.name"));
pub static BOB: Lazy<&UserId> = Lazy::new(|| user_id!("@bob:other.server"));
pub static CAROL: Lazy<&UserId> = Lazy::new(|| user_id!("@carol:other.server"));

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

    let response = Response::builder().body(data.to_string().as_bytes().to_vec()).unwrap();
    SyncResponse::try_from_http_response(response).unwrap()
}

pub fn response_from_file(json: &JsonValue) -> Response<Vec<u8>> {
    Response::builder().status(200).body(json.to_string().as_bytes().to_vec()).unwrap()
}
