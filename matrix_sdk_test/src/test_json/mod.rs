//! Test data for the matrix-sdk crates.
//!
//! Exporting each const allows all the test data to have a single source of truth.
//! When running `cargo publish` no external folders are allowed so all the
//! test data needs to be contained within this crate.

pub mod events;
pub mod sync;

pub use events::{
    ALIAS, ALIASES, MEMBER, MESSAGE_EDIT, MESSAGE_TEXT, NAME, POWER_LEVELS, PRESENCE, REACTION,
    TYPING,
};
pub use sync::{DEFAULT_SYNC, INVITE_SYNC, LEAVE_SYNC, MORE_SYNC, SYNC};
