//! Test data for the matrix-sdk crates.
//!
//! Exporting each const allows all the test data to have a single source of
//! truth. When running `cargo publish` no external folders are allowed so all
//! the test data needs to be contained within this crate.

use lazy_static::lazy_static;
use serde_json::{json, Value as JsonValue};

pub mod events;
pub mod members;
pub mod sync;

pub use events::{
    ALIAS, ALIASES, ENCRYPTION, EVENT_ID, KEYS_QUERY, KEYS_UPLOAD, LOGIN, LOGIN_RESPONSE_ERR,
    LOGIN_TYPES, LOGIN_WITH_DISCOVERY, LOGOUT, MEMBER, MEMBER_INVITE, MEMBER_NAME_CHANGE,
    MEMBER_STRIPPED, MESSAGE_EDIT, MESSAGE_TEXT, NAME, NAME_STRIPPED, POWER_LEVELS, PRESENCE,
    PUBLIC_ROOMS, PUSH_RULES, REACTION, READ_RECEIPT, READ_RECEIPT_OTHER, REDACTED,
    REDACTED_INVALID, REDACTED_STATE, REDACTION, REGISTRATION_RESPONSE_ERR, ROOM_ID, ROOM_MESSAGES,
    SYNC_ROOM_MESSAGES_BATCH_1, SYNC_ROOM_MESSAGES_BATCH_2, TAG, TOPIC, TYPING,
};
pub use members::MEMBERS;
pub use sync::{
    DEFAULT_SYNC_SUMMARY, INVITE_SYNC, LEAVE_SYNC, LEAVE_SYNC_EVENT, MORE_SYNC, MORE_SYNC_2, SYNC,
    VOIP_SYNC,
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

lazy_static! {
    pub static ref WELL_KNOWN: JsonValue = json!({
        "m.homeserver": {
            "base_url": "HOMESERVER_URL"
        }
    });
}

lazy_static! {
    pub static ref VERSIONS: JsonValue = json!({
        "versions": [
            "r0.0.1",
            "r0.1.0",
            "r0.2.0",
            "r0.3.0",
            "r0.4.0",
            "r0.5.0",
            "r0.6.0"
        ],
        "unstable_features": {
            "org.matrix.label_based_filtering":true,
            "org.matrix.e2e_cross_signing":true
        }
    });
}

lazy_static! {
    pub static ref WHOAMI: JsonValue = json!({
        "user_id": "@joe:example.org"
    });
}
