//! Test data for the matrix-sdk crates.
//!
//! Exporting each const allows all the test data to have a single source of truth.
//! When running `cargo publish` no external folders are allowed so all the
//! test data needs to be contained within this crate.

pub mod events;
pub mod sync;

pub use events::{
    ALIAS, ALIASES, EVENT_ID, KEYS_QUERY, KEYS_UPLOAD, LOGIN, LOGIN_RESPONSE_ERR, LOGOUT, MEMBER,
    MESSAGE_EDIT, MESSAGE_TEXT, NAME, POWER_LEVELS, PRESENCE, REACTION, REGISTRATION_RESPONSE_ERR,
    ROOM_ID, TYPING,
};
pub use sync::{DEFAULT_SYNC_SUMMARY, INVITE_SYNC, LEAVE_SYNC, LEAVE_SYNC_EVENT, MORE_SYNC, SYNC};
