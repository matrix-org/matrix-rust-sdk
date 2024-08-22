use http::Response;
pub use matrix_sdk_test_macros::async_test;
use once_cell::sync::Lazy;
use ruma::{
    api::{client::sync::sync_events::v3::Response as SyncResponse, IncomingResponse},
    room_id, user_id, RoomId, UserId,
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
            .cast::<::ruma::events::AnyMessageLikeEventContent>()
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

/// Create a `Raw<AnySyncStateEvent>` from arbitrary JSON.
///
/// Forwards all arguments to [`serde_json::json`].
#[macro_export]
macro_rules! sync_state_event {
    ($( $tt:tt )*) => {
        ::ruma::serde::Raw::new(&::serde_json::json!( $($tt)* ))
            .unwrap()
            .cast::<::ruma::events::AnySyncStateEvent>()
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
            .cast::<::ruma::events::AnyStrippedStateEvent>()
    }
}

/// Initialize a tracing subscriber if the target architecture is not WASM.
///
/// Uses a sensible default filter that can be overridden through the `RUST_LOG`
/// environment variable and runs once before all tests by using the [`ctor`]
/// crate.
///
/// Invoke this macro once per compilation unit (`lib.rs`, `tests/*.rs`,
/// `tests/*/main.rs`).
#[macro_export]
macro_rules! init_tracing_for_tests {
    () => {
        #[cfg(not(target_arch = "wasm32"))]
        #[$crate::__macro_support::ctor]
        fn init_logging() {
            use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
            use $crate::__macro_support::tracing_subscriber;

            tracing_subscriber::registry()
                .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    // Output is only printed for failing tests, but still we shouldn't overload
                    // the output with unnecessary info. When debugging a specific test, it's easy
                    // to override this default by setting the `RUST_LOG` environment variable.
                    //
                    // Since tracing_subscriber does prefix matching, the `matrix_sdk=` directive
                    // takes effect for all the main crates (`matrix_sdk_base`, `matrix_sdk_crypto`
                    // and so on).
                    "info,matrix_sdk=debug".into()
                }))
                .with(tracing_subscriber::fmt::layer().with_test_writer())
                .init();
        }
    };
}

#[doc(hidden)]
pub mod __macro_support {
    #[cfg(not(target_arch = "wasm32"))]
    pub use ctor::ctor;
    #[cfg(not(target_arch = "wasm32"))]
    pub use tracing_subscriber;
}

mod event_builder;

#[cfg(not(target_arch = "wasm32"))]
pub mod mocks;

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
