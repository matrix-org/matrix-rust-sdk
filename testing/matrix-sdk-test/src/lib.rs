use http::Response;
pub use matrix_sdk_test_macros::async_test;
use ruma::api::{client::sync::sync_events::v3::Response as SyncResponse, IncomingResponse};
use serde_json::Value as JsonValue;

pub mod notification_settings;
mod sync_builder;
pub mod test_json;

pub use sync_builder::{
    bulk_room_members, EphemeralTestEvent, GlobalAccountDataTestEvent, InvitedRoomBuilder,
    JoinedRoomBuilder, LeftRoomBuilder, PresenceTestEvent, RoomAccountDataTestEvent,
    StateTestEvent, StrippedStateTestEvent, SyncResponseBuilder, TimelineTestEvent,
};

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
