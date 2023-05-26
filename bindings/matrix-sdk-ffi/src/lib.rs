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

mod platform;

pub mod authentication_service;
pub mod client;
pub mod client_builder;
mod error;
pub mod event;
mod helpers;
pub mod notification;
pub mod room;
pub mod room_member;
pub mod session_verification;
pub mod sliding_sync;
pub mod timeline;
pub mod tracing;

use async_compat::TOKIO1 as RUNTIME;
pub use matrix_sdk::ruma::{api::client::account::register, UserId};
pub use matrix_sdk_ui::timeline::PaginationOutcome;
pub use platform::*;

pub use self::{
    authentication_service::*, client::*, event::*, notification::*, room::*, room_member::*,
    session_verification::*, sliding_sync::*, timeline::*, tracing::*,
};
// Re-exports for more convenient use inside other submodules
use self::{client::Client, error::ClientError};

uniffi::include_scaffolding!("api");

#[uniffi::export]
pub fn sdk_git_sha() -> String {
    env!("VERGEN_GIT_SHA").to_owned()
}
