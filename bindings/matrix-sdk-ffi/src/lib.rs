// TODO: target-os conditional would be good.

#![allow(unused_qualifications)]

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
mod helpers;
pub mod room;
pub mod session_verification;
pub mod sliding_sync;
pub mod timeline;
mod uniffi_api;

use client::Client;
use client_builder::ClientBuilder;
use once_cell::sync::Lazy;
use tokio::runtime::Runtime;
pub use uniffi_api::*;

pub static RUNTIME: Lazy<Runtime> =
    Lazy::new(|| Runtime::new().expect("Can't start Tokio runtime"));

pub use matrix_sdk::{
    room::timeline::PaginationOutcome,
    ruma::{api::client::account::register, UserId},
};

pub use self::{
    authentication_service::*, client::*, room::*, session_verification::*, sliding_sync::*,
    timeline::*,
};

#[derive(Default, Debug)]
pub struct ClientState {
    has_first_synced: bool,
    is_syncing: bool,
    should_stop_syncing: bool,
    is_soft_logout: bool,
}

#[derive(thiserror::Error, Debug)]
pub enum ClientError {
    #[error("client error: {msg}")]
    Generic { msg: String },
}

impl From<anyhow::Error> for ClientError {
    fn from(e: anyhow::Error) -> ClientError {
        ClientError::Generic { msg: e.to_string() }
    }
}

pub use platform::*;

mod uniffi_types {
    pub use matrix_sdk::ruma::events::room::{message::RoomMessageEventContent, MediaSource};

    pub use crate::{
        authentication_service::{
            AuthenticationError, AuthenticationService, HomeserverLoginDetails,
        },
        client::Client,
        client_builder::ClientBuilder,
        room::{Membership, MembershipState, Room, RoomMember},
        session_verification::{SessionVerificationController, SessionVerificationEmoji},
        sliding_sync::{
            RequiredState, RoomListEntry, SlidingSync, SlidingSyncBuilder,
            SlidingSyncRequestListFilters, SlidingSyncRoom, SlidingSyncView,
            SlidingSyncViewBuilder, StoppableSpawn, UnreadNotificationsCount,
        },
        timeline::{
            EmoteMessageContent, EncryptedMessage, EventSendState, EventTimelineItem, FileInfo,
            FileMessageContent, FormattedBody, ImageInfo, ImageMessageContent, InsertAtData,
            MembershipChange, Message, MessageFormat, MessageType, NoticeMessageContent,
            OtherState, Profile, Reaction, TextMessageContent, ThumbnailInfo, TimelineChange,
            TimelineDiff, TimelineItem, TimelineItemContent, TimelineItemContentKind, UpdateAtData,
            VideoInfo, VideoMessageContent, VirtualTimelineItem,
        },
    };
}
