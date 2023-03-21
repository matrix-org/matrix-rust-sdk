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
mod helpers;
pub mod notification_service;
pub mod room;
pub mod room_member;
pub mod session_verification;
pub mod sliding_sync;
pub mod timeline;

use client::Client;
use client_builder::ClientBuilder;
use matrix_sdk::{encryption::CryptoStoreError, HttpError, IdParseError};
use once_cell::sync::Lazy;
use tokio::runtime::Runtime;

pub static RUNTIME: Lazy<Runtime> =
    Lazy::new(|| Runtime::new().expect("Can't start Tokio runtime"));

pub use matrix_sdk::{
    room::timeline::PaginationOutcome,
    ruma::{api::client::account::register, UserId},
};

pub use self::{
    authentication_service::*, client::*, notification_service::*, room::*, room_member::*,
    session_verification::*, sliding_sync::*, timeline::*,
};

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

impl From<matrix_sdk::Error> for ClientError {
    fn from(e: matrix_sdk::Error) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<CryptoStoreError> for ClientError {
    fn from(e: CryptoStoreError) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<HttpError> for ClientError {
    fn from(e: HttpError) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<IdParseError> for ClientError {
    fn from(e: IdParseError) -> Self {
        anyhow::Error::from(e).into()
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(e: serde_json::Error) -> Self {
        anyhow::Error::from(e).into()
    }
}

pub use platform::*;

uniffi::include_scaffolding!("api");

mod uniffi_types {
    pub use matrix_sdk::ruma::events::room::{message::RoomMessageEventContent, MediaSource};

    pub use crate::{
        authentication_service::{
            AuthenticationError, AuthenticationService, HomeserverLoginDetails,
        },
        client::{
            Client, CreateRoomParameters, HttpPusherData, PushFormat, PusherIdentifiers,
            PusherKind, SearchUsersResults, Session, UserProfile,
        },
        client_builder::ClientBuilder,
        room::{Membership, Room},
        room_member::{MembershipState, RoomMember},
        session_verification::{SessionVerificationController, SessionVerificationEmoji},
        sliding_sync::{
            RequiredState, RoomListEntry, SlidingSync, SlidingSyncBuilder, SlidingSyncList,
            SlidingSyncListBuilder, SlidingSyncRequestListFilters, SlidingSyncRoom, TaskHandle,
            UnreadNotificationsCount,
        },
        timeline::{
            AudioInfo, AudioMessageContent, EmoteMessageContent, EncryptedMessage, EventSendState,
            EventTimelineItem, FileInfo, FileMessageContent, FormattedBody, ImageInfo,
            ImageMessageContent, InsertData, MembershipChange, Message, MessageFormat, MessageType,
            NoticeMessageContent, OtherState, ProfileTimelineDetails, Reaction, SetData,
            TextMessageContent, ThumbnailInfo, TimelineChange, TimelineDiff, TimelineItem,
            TimelineItemContent, TimelineItemContentKind, VideoInfo, VideoMessageContent,
            VirtualTimelineItem,
        },
        ClientError,
    };
}
