// TODO: target-os conditional would be good.

#![allow(unused_qualifications, clippy::new_without_default)]

macro_rules! unwrap_or_clone_arc_into_variant {
    (
        $arc:ident $(, .$field:tt)?, $pat:pat => $body:expr
    ) => {
        #[allow(unused_variables)]
        match &(*$arc)$(.$field)? {
            $pat => {
                #[warn(unused_variables)]
                match crate::helpers::unwrap_or_clone_arc($arc)$(.$field)? {
                    $pat => Some($body),
                    _ => unreachable!(),
                }
            },
            _ => None,
        }
    };
}

mod authentication_service;
mod client;
mod client_builder;
mod encryption_sync;
mod error;
mod event;
mod helpers;
mod notification;
mod platform;
mod room;
mod room_list;
mod room_member;
mod session_verification;
mod task_handle;
mod timeline;
mod tracing;

use async_compat::TOKIO1 as RUNTIME;
use matrix_sdk::ruma::events::room::{message::RoomMessageEventContent, MediaSource};
use matrix_sdk_ui::timeline::{BackPaginationStatus, EventItemOrigin};

use self::{
    client::{CreateRoomParameters, RoomPreset, RoomVisibility},
    error::ClientError,
    session_verification::{SessionVerificationControllerDelegate, SessionVerificationEmoji},
    task_handle::TaskHandle,
    timeline::MediaSourceExt,
};

uniffi::include_scaffolding!("api");

#[uniffi::export]
fn sdk_git_sha() -> String {
    env!("VERGEN_GIT_SHA").to_owned()
}
