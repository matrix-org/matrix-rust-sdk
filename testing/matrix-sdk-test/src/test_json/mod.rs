//! Test data for the matrix-sdk crates.
//!
//! Exporting each const allows all the test data to have a single source of
//! truth. When running `cargo publish` no external folders are allowed so all
//! the test data needs to be contained within this crate.

use once_cell::sync::Lazy;
use serde_json::{json, Value as JsonValue};

pub mod api_responses;
pub mod members;
pub mod messages;
pub mod search_users;
pub mod sync;
pub mod sync_events;

pub use api_responses::{
    DEVICES, GET_ALIAS, KEYS_QUERY, KEYS_QUERY_TWO_DEVICES_ONE_SIGNED, KEYS_UPLOAD, LOGIN,
    LOGIN_RESPONSE_ERR, LOGIN_TYPES, LOGIN_WITH_DISCOVERY, LOGIN_WITH_REFRESH_TOKEN, NOT_FOUND,
    PUBLIC_ROOMS, REFRESH_TOKEN, REFRESH_TOKEN_WITH_REFRESH_TOKEN, REGISTRATION_RESPONSE_ERR,
    UNKNOWN_TOKEN_SOFT_LOGOUT, VERSIONS, WELL_KNOWN, WHOAMI,
};
pub use members::MEMBERS;
pub use messages::{ROOM_MESSAGES, ROOM_MESSAGES_BATCH_1, ROOM_MESSAGES_BATCH_2};
pub use sync::{
    DEFAULT_SYNC_ROOM_ID, DEFAULT_SYNC_SUMMARY, INVITE_SYNC, LEAVE_SYNC, LEAVE_SYNC_EVENT,
    MORE_SYNC, MORE_SYNC_2, SYNC, VOIP_SYNC,
};
pub use sync_events::{
    ALIAS, ALIASES, DIRECT, ENCRYPTION, MEMBER, MEMBER_ADDITIONAL, MEMBER_BAN, MEMBER_INVITE,
    MEMBER_LEAVE, MEMBER_NAME_CHANGE, MEMBER_STRIPPED, NAME, NAME_STRIPPED, POWER_LEVELS, PRESENCE,
    PUSH_RULES, READ_RECEIPT, READ_RECEIPT_OTHER, REDACTED_INVALID, REDACTED_STATE, TAG, TOPIC,
    TOPIC_REDACTION, TYPING,
};

/// An empty response.
pub static EMPTY: Lazy<JsonValue> = Lazy::new(|| json!({}));

/// A response with only an event ID.
pub static EVENT_ID: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "event_id": "$h29iv0s8:example.com"
    })
});

/// A response with only a room ID.
pub static ROOM_ID: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "room_id": "!testroom:example.org"
    })
});
