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
mod live_location_share;
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
mod tracing;
mod utils;
mod widget;

use matrix_sdk::ruma::events::room::message::RoomMessageEventContentWithoutRelation;
use once_cell::sync::Lazy;

fn init_tokio_runtime() {
    let _ = *RUNTIME;
}

static RUNTIME: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    let has_runtime = tokio::runtime::Handle::try_current().is_ok();
    assert!(!has_runtime, "must call `init_tokio_runtime()` before anything else");

    eprintln!("spawning our own multithreaded runtime!");

    // Get the number of available cores through the system, if possible.
    let num_available_cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);

    // The number of worker threads will be either that or 4, whichever is smaller.
    let num_worker_threads = usize::min(num_available_cores, 4);

    // Create a multithreaded runtime with this number of worker threads, each
    // having access to 2 MiB of stack space (this stack space size is the
    // default, but we keep it explicit here).
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(num_worker_threads)
        .thread_stack_size(2 * 1024 * 1024)
        .build()
        .expect("cannot init tokio runtime")
});

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
