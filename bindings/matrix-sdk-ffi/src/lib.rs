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
mod chunk_iterator;
mod client;
mod client_builder;
mod error;
mod event;
mod helpers;
mod notification;
mod notification_settings;
mod platform;
mod room;
mod room_info;
mod room_list;
mod room_member;
mod ruma;
mod session_verification;
mod sync_service;
mod task_handle;
mod timeline;
mod tracing;
mod utils;
mod widget;

use async_compat::TOKIO1 as RUNTIME;
use matrix_sdk::ruma::events::room::{
    message::RoomMessageEventContentWithoutRelation, MediaSource,
};
use matrix_sdk_ui::timeline::{BackPaginationStatus, EventItemOrigin};

use self::{
    error::ClientError,
    ruma::{MediaSourceExt, Mentions, RoomMessageEventContentWithoutRelationExt},
    task_handle::TaskHandle,
};

uniffi::include_scaffolding!("api");

#[uniffi::export]
fn sdk_git_sha() -> String {
    env!("VERGEN_GIT_SHA").to_owned()
}
