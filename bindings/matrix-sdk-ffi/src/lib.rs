#![recursion_limit = "256"]
#![allow(unused_qualifications, clippy::new_without_default)]
// Needed because uniffi macros contain empty lines after docs.
#![allow(clippy::empty_line_after_doc_comments)]
// Needed because uniffi generates a big const array.
#![allow(clippy::large_const_arrays)]

mod authentication;
mod chunk_iterator;
mod client;
mod client_builder;
mod content_scanner;
mod encryption;
mod error;
mod event;
mod helpers;
mod identity_status_change;
mod live_locations_observer;
mod notification;
mod notification_settings;
mod password_strength;
mod platform;
mod qr_code;
mod room;
mod room_alias;
mod room_directory_search;
mod room_list;
mod room_member;
mod room_preview;
mod ruma;
mod runtime;
#[cfg(feature = "experimental-search")]
mod search_service;
mod session_verification;
mod spaces;
mod store;
mod sync_service;
mod sync_v2;
mod task_handle;
mod timeline;
mod utd;
mod utils;
mod widget;

use matrix_sdk::ruma::events::room::message::RoomMessageEventContentWithoutRelation;

use self::{
    error::ClientError,
    ruma::{Mentions, RoomMessageEventContentWithoutRelationExt},
    task_handle::TaskHandle,
};

uniffi::include_scaffolding!("api");

#[matrix_sdk_ffi_macros::export]
fn sdk_git_sha() -> String {
    env!("VERGEN_GIT_SHA").to_owned()
}
