//! Test data for the matrix-sdk crates.
//!
//! Exporting each const allows all the test data to have a single source of truth.
//! When running `cargo publish` no external folders are allowed so all the
//! test data needs to be contained within this crate.

use lazy_static::lazy_static;
use serde_json::{json, Value as JsonValue};

pub mod events;
pub mod sync;

pub use events::{
    ALIAS, ALIASES, EVENT_ID, KEYS_QUERY, KEYS_UPLOAD, LOGIN, LOGIN_RESPONSE_ERR, LOGOUT, MEMBER,
    MEMBER_NAME_CHANGE, MESSAGE_EDIT, MESSAGE_TEXT, NAME, POWER_LEVELS, PRESENCE, PUBLIC_ROOMS,
    REACTION, REDACTED, REDACTED_INVALID, REDACTED_STATE, REDACTION, REGISTRATION_RESPONSE_ERR,
    ROOM_ID, ROOM_MESSAGES, TYPING,
};
pub use sync::{
    DEFAULT_SYNC_SUMMARY, INVITE_SYNC, LEAVE_SYNC, LEAVE_SYNC_EVENT, MORE_SYNC, SYNC, VOIP_SYNC,
};

lazy_static! {
    pub static ref DEVICES: JsonValue = json!({
        "devices": [
            {
                "device_id": "BNYQQWUMXO",
                "display_name": "Client 1",
                "last_seen_ip": "-",
                "last_seen_ts": 1596117733037u64,
                "user_id": "@example:localhost"
            },
            {
                "device_id": "LEBKSEUSNR",
                "display_name": "Client 2",
                "last_seen_ip": "-",
                "last_seen_ts": 1599057006985u64,
                "user_id": "@example:localhost"
            }
        ]
    });
}
