//! Test data for the matrix-sdk crates.
//!
//! Exporting each const allows all the test data to have a single source of
//! truth. When running `cargo publish` no external folders are allowed so all
//! the test data needs to be contained within this crate.

use std::sync::LazyLock;

use serde_json::{Value as JsonValue, json};

use crate::DEFAULT_TEST_ROOM_ID;

pub mod api_responses;
pub mod keys_query;
pub mod keys_query_sets;
pub mod members;
pub mod sync;

pub use api_responses::{
    KEYS_QUERY, KEYS_QUERY_TWO_DEVICES_ONE_SIGNED, KEYS_UPLOAD, LOGIN, LOGIN_RESPONSE_ERR,
    LOGIN_TYPES, LOGIN_WITH_DISCOVERY, LOGIN_WITH_REFRESH_TOKEN, NOT_FOUND, PUBLIC_ROOMS,
    PUBLIC_ROOMS_FINAL_PAGE, REFRESH_TOKEN, REFRESH_TOKEN_WITH_REFRESH_TOKEN,
    REGISTRATION_RESPONSE_ERR, UNKNOWN_TOKEN_SOFT_LOGOUT, VERSIONS, WHOAMI,
};
pub use members::MEMBERS;
pub use sync::{DEFAULT_SYNC_SUMMARY, INVITE_SYNC, LEAVE_SYNC, SYNC};

/// An empty response.
pub static EMPTY: LazyLock<JsonValue> = LazyLock::new(|| json!({}));
