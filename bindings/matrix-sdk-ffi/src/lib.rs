// TODO: target-os conditional would be good.

#![allow(unused_qualifications, clippy::new_without_default)]
#![allow(clippy::empty_line_after_doc_comments)] // Needed because uniffi macros contain empty
                                                 // lines after docs.

mod authentication;
mod chunk_iterator;
mod client;
mod client_builder;
mod element;
mod encryption;
mod error;
mod event;
mod helpers;
mod identity_status_change;
mod notification;
mod notification_settings;
mod platform;
mod room;
mod room_alias;
mod room_directory_search;
mod room_info;
mod room_list;
mod room_member;
mod room_preview;
mod ruma;
mod session_verification;
mod sync_service;
mod task_handle;
mod timeline;
mod timeline_event_filter;
mod tracing;
mod utils;
mod widget;

use async_compat::TOKIO1 as RUNTIME;
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
