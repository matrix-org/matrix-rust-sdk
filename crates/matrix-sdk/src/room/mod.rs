// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! High-level room API

use std::{
    borrow::Borrow,
    collections::{BTreeMap, HashMap},
    future::Future,
    ops::Deref,
    sync::Arc,
    time::Duration,
};

use async_stream::stream;
use eyeball::SharedObservable;
use futures_core::Stream;
use futures_util::{
    StreamExt, future::join_all, stream as futures_stream, stream::FuturesUnordered,
};
use http::StatusCode;
#[cfg(feature = "e2e-encryption")]
pub use identity_status_changes::IdentityStatusChanges;
#[cfg(feature = "experimental-encrypted-state-events")]
use matrix_sdk_base::crypto::types::events::room::encrypted::EncryptedEvent;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::crypto::{
    IdentityStatusChange, RoomIdentityProvider, UserIdentity, types::events::CryptoContextInfo,
};
pub use matrix_sdk_base::store::StoredThreadSubscription;
use matrix_sdk_base::{
    ComposerDraft, EncryptionState, RoomInfoNotableUpdateReasons, RoomMemberships, SendOutsideWasm,
    StateChanges, StateStoreDataKey, StateStoreDataValue,
    deserialized_responses::{
        RawAnySyncOrStrippedState, RawSyncOrStrippedState, SyncOrStrippedState,
    },
    media::{MediaThumbnailSettings, store::IgnoreMediaRetentionPolicy},
    store::{StateStoreExt, ThreadSubscriptionStatus},
};
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::{crypto::RoomEventDecryptionResult, deserialized_responses::EncryptionInfo};
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_common::BoxFuture;
use matrix_sdk_common::{
    deserialized_responses::TimelineEvent,
    executor::{JoinHandle, spawn},
    timeout::timeout,
};
#[cfg(feature = "experimental-search")]
use matrix_sdk_search::error::IndexError;
#[cfg(feature = "experimental-search")]
#[cfg(doc)]
use matrix_sdk_search::index::RoomIndex;
use mime::Mime;
use reply::Reply;
#[cfg(any(feature = "experimental-search", feature = "e2e-encryption"))]
use ruma::events::AnySyncMessageLikeEvent;
#[cfg(feature = "experimental-encrypted-state-events")]
use ruma::events::AnySyncStateEvent;
#[cfg(feature = "unstable-msc4274")]
use ruma::events::room::message::GalleryItemType;
#[cfg(feature = "e2e-encryption")]
use ruma::events::{
    AnySyncTimelineEvent, SyncMessageLikeEvent, room::encrypted::OriginalSyncRoomEncryptedEvent,
};
use ruma::{
    EventId, Int, MatrixToUri, MatrixUri, MxcUri, OwnedEventId, OwnedRoomId, OwnedServerName,
    OwnedTransactionId, OwnedUserId, RoomId, TransactionId, UInt, UserId,
    api::client::{
        config::{set_global_account_data, set_room_account_data},
        context,
        error::ErrorKind,
        filter::LazyLoadOptions,
        membership::{
            Invite3pid, ban_user, forget_room, get_member_events,
            invite_user::{self, v3::InvitationRecipient},
            kick_user, leave_room, unban_user,
        },
        message::send_message_event,
        read_marker::set_read_marker,
        receipt::create_receipt,
        redact::redact_event,
        room::{get_room_event, report_content, report_room},
        state::{get_state_event_for_key, send_state_event},
        tag::{create_tag, delete_tag},
        threads::{get_thread_subscription, subscribe_thread, unsubscribe_thread},
        typing::create_typing_event::{self, v3::Typing},
    },
    assign,
    events::{
        AnyRoomAccountDataEvent, AnyRoomAccountDataEventContent, AnyTimelineEvent, EmptyStateKey,
        Mentions, MessageLikeEventContent, OriginalSyncStateEvent, RedactContent,
        RedactedStateEventContent, RoomAccountDataEvent, RoomAccountDataEventContent,
        RoomAccountDataEventType, StateEventContent, StateEventType, StaticEventContent,
        StaticStateEventContent, SyncStateEvent,
        beacon::BeaconEventContent,
        beacon_info::BeaconInfoEventContent,
        direct::DirectEventContent,
        marked_unread::MarkedUnreadEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::{
            ImageInfo, MediaSource, ThumbnailInfo,
            avatar::{self, RoomAvatarEventContent},
            encryption::RoomEncryptionEventContent,
            history_visibility::HistoryVisibility,
            member::{MembershipChange, RoomMemberEventContent, SyncRoomMemberEvent},
            message::{
                AudioInfo, AudioMessageEventContent, FileInfo, FileMessageEventContent,
                ImageMessageEventContent, MessageType, RoomMessageEventContent,
                TextMessageEventContent, UnstableAmplitude, UnstableAudioDetailsContentBlock,
                UnstableVoiceContentBlock, VideoInfo, VideoMessageEventContent,
            },
            name::RoomNameEventContent,
            pinned_events::RoomPinnedEventsEventContent,
            power_levels::{
                RoomPowerLevels, RoomPowerLevelsEventContent, RoomPowerLevelsSource, UserPowerLevel,
            },
            server_acl::RoomServerAclEventContent,
            topic::RoomTopicEventContent,
        },
        space::{child::SpaceChildEventContent, parent::SpaceParentEventContent},
        tag::{TagInfo, TagName},
        typing::SyncTypingEvent,
    },
    int,
    push::{Action, AnyPushRuleRef, PushConditionRoomCtx, Ruleset},
    serde::Raw,
    time::Instant,
};
#[cfg(feature = "experimental-encrypted-state-events")]
use ruma::{
    events::room::encrypted::unstable_state::OriginalSyncStateRoomEncryptedEvent,
    serde::JsonCastable,
};
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::{join, sync::broadcast};
use tracing::{debug, error, info, instrument, trace, warn};

use self::futures::{SendAttachment, SendMessageLikeEvent, SendRawMessageLikeEvent};
pub use self::{
    member::{RoomMember, RoomMemberRole},
    messages::{
        EventWithContextResponse, IncludeRelations, ListThreadsOptions, Messages, MessagesOptions,
        Relations, RelationsOptions, ThreadRoots,
    },
};
#[cfg(feature = "e2e-encryption")]
use crate::encryption::backups::BackupState;
#[cfg(doc)]
use crate::event_cache::EventCache;
#[cfg(feature = "experimental-encrypted-state-events")]
use crate::room::futures::{SendRawStateEvent, SendStateEvent};
use crate::{
    BaseRoom, Client, Error, HttpResult, Result, RoomState, TransmissionProgress,
    attachment::{AttachmentConfig, AttachmentInfo},
    client::WeakClient,
    config::RequestConfig,
    error::{BeaconError, WrongRoomState},
    event_cache::{self, EventCacheDropHandles, RoomEventCache},
    event_handler::{EventHandler, EventHandlerDropGuard, EventHandlerHandle, SyncEvent},
    live_location_share::ObservableLiveLocation,
    media::{MediaFormat, MediaRequestParameters},
    notification_settings::{IsEncrypted, IsOneToOne, RoomNotificationMode},
    room::{
        knock_requests::{KnockRequest, KnockRequestMemberInfo},
        power_levels::{RoomPowerLevelChanges, RoomPowerLevelsExt},
        privacy_settings::RoomPrivacySettings,
    },
    sync::RoomUpdate,
    utils::{IntoRawMessageLikeEventContent, IntoRawStateEventContent},
};

pub mod edit;
pub mod futures;
pub mod identity_status_changes;
/// Contains code related to requests to join a room.
pub mod knock_requests;
mod member;
mod messages;
pub mod power_levels;
pub mod reply;

pub mod calls;

/// Contains all the functionality for modifying the privacy settings in a room.
pub mod privacy_settings;

#[cfg(feature = "e2e-encryption")]
pub(crate) mod shared_room_history;

/// A struct containing methods that are common for Joined, Invited and Left
/// Rooms
#[derive(Debug, Clone)]
pub struct Room {
    inner: BaseRoom,
    pub(crate) client: Client,
}

impl Deref for Room {
    type Target = BaseRoom;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

const TYPING_NOTICE_TIMEOUT: Duration = Duration::from_secs(4);
const TYPING_NOTICE_RESEND_TIMEOUT: Duration = Duration::from_secs(3);

/// A thread subscription, according to the semantics of MSC4306.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadSubscription {
    /// Whether the subscription was made automatically by a client, not by
    /// manual user choice.
    pub automatic: bool,
}

/// Context allowing to compute the push actions for a given event.
#[derive(Debug)]
pub struct PushContext {
    /// The Ruma context used to compute the push actions.
    push_condition_room_ctx: PushConditionRoomCtx,

    /// Push rules for this room, based on the push rules state event, or the
    /// global server default as defined by [`Ruleset::server_default`].
    push_rules: Ruleset,
}

impl PushContext {
    /// Create a new [`PushContext`] from its inner components.
    pub fn new(push_condition_room_ctx: PushConditionRoomCtx, push_rules: Ruleset) -> Self {
        Self { push_condition_room_ctx, push_rules }
    }

    /// Compute the push rules for a given event.
    pub async fn for_event<T>(&self, event: &Raw<T>) -> Vec<Action> {
        self.push_rules.get_actions(event, &self.push_condition_room_ctx).await.to_owned()
    }

    /// Compute the push rules for a given event, with extra logging to help
    /// debugging.
    #[doc(hidden)]
    #[instrument(skip_all)]
    pub async fn traced_for_event<T>(&self, event: &Raw<T>) -> Vec<Action> {
        let rules = self
            .push_rules
            .iter()
            .filter_map(|r| {
                if !r.enabled() {
                    return None;
                }

                let simplified_action = if r.actions().is_empty() { "inhibit" } else { "notify" };

                let conditions = match r {
                    AnyPushRuleRef::Override(r) => {
                        format!("{:?}", r.conditions)
                    }
                    AnyPushRuleRef::Content(r) => format!("content-body-match:{}", r.pattern),
                    AnyPushRuleRef::Room(r) => format!("room-match:{}", r.rule_id),
                    AnyPushRuleRef::Sender(r) => format!("sender-match:{}", r.rule_id),
                    AnyPushRuleRef::Underride(r) => format!("{:?}", r.conditions),
                    _ => "<unknown push rule kind>".to_owned(),
                };

                Some(format!("- {}: {conditions} => {simplified_action}", r.rule_id(),))
            })
            .collect::<Vec<_>>()
            .join("\n");
        trace!("rules:\n\n{rules}\n\n");

        let found = self.push_rules.get_match(event, &self.push_condition_room_ctx).await;

        if let Some(found) = found {
            trace!("rule {} matched", found.rule_id());
            found.actions().to_owned()
        } else {
            trace!("no match");
            Vec::new()
        }
    }
}

macro_rules! make_media_type {
    ($t:ty, $content_type: ident, $filename: ident, $source: ident, $caption: ident, $info: ident, $thumbnail: ident) => {{
        // If caption is set, use it as body, and filename as the file name; otherwise,
        // body is the filename, and the filename is not set.
        // https://github.com/matrix-org/matrix-spec-proposals/blob/main/proposals/2530-body-as-caption.md
        let (body, formatted, filename) = match $caption {
            Some(TextMessageEventContent { body, formatted, .. }) => (body, formatted, Some($filename)),
            None => ($filename, None, None),
        };

        let (thumbnail_source, thumbnail_info) = $thumbnail.unzip();

        match $content_type.type_() {
            mime::IMAGE => {
                let info = assign!($info.map(ImageInfo::from).unwrap_or_default(), {
                    mimetype: Some($content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info
                });
                let content = assign!(ImageMessageEventContent::new(body, $source), {
                    info: Some(Box::new(info)),
                    formatted,
                    filename
                });
                <$t>::Image(content)
            }

            mime::AUDIO => {
                let mut content = assign!(AudioMessageEventContent::new(body, $source), {
                    formatted,
                    filename
                });

                if let Some(AttachmentInfo::Audio(audio_info) | AttachmentInfo::Voice(audio_info)) = &$info &&
                 let Some(duration) = audio_info.duration && let Some(waveform_vec) = &audio_info.waveform {
                    let waveform = waveform_vec
                        .iter()
                        .map(|v| ((*v).clamp(0.0, 1.0) * UnstableAmplitude::MAX as f32) as u16)
                        .map(Into::into)
                        .collect();
                    content.audio =
                        Some(UnstableAudioDetailsContentBlock::new(duration, waveform));
                }

                if matches!($info, Some(AttachmentInfo::Voice(_))) {
                    content.voice = Some(UnstableVoiceContentBlock::new());
                }

                let mut audio_info = $info.map(AudioInfo::from).unwrap_or_default();
                audio_info.mimetype = Some($content_type.as_ref().to_owned());
                let content = content.info(Box::new(audio_info));

                <$t>::Audio(content)
            }

            mime::VIDEO => {
                let info = assign!($info.map(VideoInfo::from).unwrap_or_default(), {
                    mimetype: Some($content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info
                });
                let content = assign!(VideoMessageEventContent::new(body, $source), {
                    info: Some(Box::new(info)),
                    formatted,
                    filename
                });
                <$t>::Video(content)
            }

            _ => {
                let info = assign!($info.map(FileInfo::from).unwrap_or_default(), {
                    mimetype: Some($content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info
                });
                let content = assign!(FileMessageEventContent::new(body, $source), {
                    info: Some(Box::new(info)),
                    formatted,
                    filename,
                });
                <$t>::File(content)
            }
        }
    }};
}

impl Room {
    /// Create a new `Room`
    ///
    /// # Arguments
    /// * `client` - The client used to make requests.
    ///
    /// * `room` - The underlying room.
    pub(crate) fn new(client: Client, room: BaseRoom) -> Self {
        Self { inner: room, client }
    }

    /// Leave this room.
    /// If the room was in [`RoomState::Invited`] state, it'll also be forgotten
    /// automatically.
    ///
    /// Only invited and joined rooms can be left.
    #[doc(alias = "reject_invitation")]
    #[instrument(skip_all, fields(room_id = ?self.inner.room_id()))]
    async fn leave_impl(&self) -> (Result<()>, &Room) {
        let state = self.state();
        if state == RoomState::Left {
            return (
                Err(Error::WrongRoomState(Box::new(WrongRoomState::new(
                    "Joined or Invited",
                    state,
                )))),
                self,
            );
        }

        // If the room was in Invited state we should also forget it when declining the
        // invite.
        let should_forget = matches!(self.state(), RoomState::Invited);

        let request = leave_room::v3::Request::new(self.inner.room_id().to_owned());
        let response = self.client.send(request).await;

        // The server can return with an error that is acceptable to ignore. Let's find
        // which one.
        if let Err(error) = response {
            #[allow(clippy::collapsible_match)]
            let ignore_error = if let Some(error) = error.client_api_error_kind() {
                match error {
                    // The user is trying to leave a room but doesn't have permissions to do so.
                    // Let's consider the user has left the room.
                    ErrorKind::Forbidden { .. } => true,
                    _ => false,
                }
            } else {
                false
            };

            error!(?error, ignore_error, should_forget, "Failed to leave the room");

            if !ignore_error {
                return (Err(error.into()), self);
            }
        }

        if let Err(e) = self.client.base_client().room_left(self.room_id()).await {
            return (Err(e.into()), self);
        }

        if should_forget {
            trace!("Trying to forget the room");

            if let Err(error) = self.forget().await {
                error!(?error, "Failed to forget the room");
            }
        }

        (Ok(()), self)
    }

    /// Leave this room and all predecessors.
    /// If any room was in [`RoomState::Invited`] state, it'll also be forgotten
    /// automatically.
    ///
    /// Only invited and joined rooms can be left.
    /// Will return an error if the current room fails to leave but
    /// will only warn if a predecessor fails to leave.
    pub async fn leave(&self) -> Result<()> {
        let mut rooms: Vec<Room> = vec![self.clone()];
        let mut current_room = self;

        while let Some(predecessor) = current_room.predecessor_room() {
            let maybe_predecessor_room = current_room.client.get_room(&predecessor.room_id);

            if let Some(predecessor_room) = maybe_predecessor_room {
                rooms.push(predecessor_room.clone());
                current_room = rooms.last().expect("Room just pushed so can't be empty");
            } else {
                warn!("Cannot find predecessor room");
                break;
            }
        }

        let batch_size = 5;

        let rooms_futures: Vec<_> = rooms
            .iter()
            .filter_map(|room| match room.state() {
                RoomState::Joined | RoomState::Invited | RoomState::Knocked => {
                    Some(room.leave_impl())
                }
                RoomState::Banned | RoomState::Left => None,
            })
            .collect();

        let mut futures_stream = futures_stream::iter(rooms_futures).buffer_unordered(batch_size);

        let mut maybe_this_room_failed_with: Option<Error> = None;

        while let Some(result) = futures_stream.next().await {
            if let (Err(e), room) = result {
                if room.room_id() == self.room_id() {
                    maybe_this_room_failed_with = Some(e);
                } else {
                    warn!("Failure while attempting to leave predecessor room: {e:?}");
                }
            }
        }

        maybe_this_room_failed_with.map_or(Ok(()), Err)
    }

    /// Join this room.
    ///
    /// Only invited and left rooms can be joined via this method.
    #[doc(alias = "accept_invitation")]
    pub async fn join(&self) -> Result<()> {
        let prev_room_state = self.inner.state();

        if prev_room_state == RoomState::Joined {
            return Err(Error::WrongRoomState(Box::new(WrongRoomState::new(
                "Invited or Left",
                prev_room_state,
            ))));
        }

        self.client.join_room_by_id(self.room_id()).await?;

        Ok(())
    }

    /// Get the inner client saved in this room instance.
    ///
    /// Returns the client this room is part of.
    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// Get the sync state of this room, i.e. whether it was fully synced with
    /// the server.
    pub fn is_synced(&self) -> bool {
        self.inner.is_state_fully_synced()
    }

    /// Gets the avatar of this room, if set.
    ///
    /// Returns the avatar.
    /// If a thumbnail is requested no guarantee on the size of the image is
    /// given.
    ///
    /// # Arguments
    ///
    /// * `format` - The desired format of the avatar.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::room_id;
    /// # use matrix_sdk::media::MediaFormat;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # async {
    /// # let user = "example";
    /// let client = Client::new(homeserver).await.unwrap();
    /// client.matrix_auth().login_username(user, "password").send().await.unwrap();
    /// let room_id = room_id!("!roomid:example.com");
    /// let room = client.get_room(&room_id).unwrap();
    /// if let Some(avatar) = room.avatar(MediaFormat::File).await.unwrap() {
    ///     std::fs::write("avatar.png", avatar);
    /// }
    /// # };
    /// ```
    pub async fn avatar(&self, format: MediaFormat) -> Result<Option<Vec<u8>>> {
        let Some(url) = self.avatar_url() else { return Ok(None) };
        let request = MediaRequestParameters { source: MediaSource::Plain(url.to_owned()), format };
        Ok(Some(self.client.media().get_media_content(&request, true).await?))
    }

    /// Sends a request to `/_matrix/client/r0/rooms/{room_id}/messages` and
    /// returns a `Messages` struct that contains a chunk of room and state
    /// events (`RoomEvent` and `AnyStateEvent`).
    ///
    /// With the encryption feature, messages are decrypted if possible. If
    /// decryption fails for an individual message, that message is returned
    /// undecrypted.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::{Client, room::MessagesOptions};
    /// # use matrix_sdk::ruma::{
    /// #     api::client::filter::RoomEventFilter,
    /// #     room_id,
    /// # };
    /// # use url::Url;
    ///
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # async {
    /// let options =
    ///     MessagesOptions::backward().from("t47429-4392820_219380_26003_2265");
    ///
    /// let mut client = Client::new(homeserver).await.unwrap();
    /// let room = client.get_room(room_id!("!roomid:example.com")).unwrap();
    /// assert!(room.messages(options).await.is_ok());
    /// # };
    /// ```
    #[instrument(skip_all, fields(room_id = ?self.inner.room_id(), ?options))]
    pub async fn messages(&self, options: MessagesOptions) -> Result<Messages> {
        let room_id = self.inner.room_id();
        let request = options.into_request(room_id);
        let http_response = self.client.send(request).await?;

        let push_ctx = self.push_context().await?;
        let chunk = join_all(
            http_response.chunk.into_iter().map(|ev| self.try_decrypt_event(ev, push_ctx.as_ref())),
        )
        .await;

        Ok(Messages {
            start: http_response.start,
            end: http_response.end,
            chunk,
            state: http_response.state,
        })
    }

    /// Register a handler for events of a specific type, within this room.
    ///
    /// This method works the same way as [`Client::add_event_handler`], except
    /// that the handler will only be called for events within this room. See
    /// that method for more details on event handler functions.
    ///
    /// `room.add_event_handler(hdl)` is equivalent to
    /// `client.add_room_event_handler(room_id, hdl)`. Use whichever one is more
    /// convenient in your use case.
    pub fn add_event_handler<Ev, Ctx, H>(&self, handler: H) -> EventHandlerHandle
    where
        Ev: SyncEvent + DeserializeOwned + Send + 'static,
        H: EventHandler<Ev, Ctx>,
    {
        self.client.add_room_event_handler(self.room_id(), handler)
    }

    /// Subscribe to all updates for this room.
    ///
    /// The returned receiver will receive a new message for each sync response
    /// that contains updates for this room.
    pub fn subscribe_to_updates(&self) -> broadcast::Receiver<RoomUpdate> {
        self.client.subscribe_to_room_updates(self.room_id())
    }

    /// Subscribe to typing notifications for this room.
    ///
    /// The returned receiver will receive a new vector of user IDs for each
    /// sync response that contains 'm.typing' event. The current user ID will
    /// be filtered out.
    pub fn subscribe_to_typing_notifications(
        &self,
    ) -> (EventHandlerDropGuard, broadcast::Receiver<Vec<OwnedUserId>>) {
        let (sender, receiver) = broadcast::channel(16);
        let typing_event_handler_handle = self.client.add_room_event_handler(self.room_id(), {
            let own_user_id = self.own_user_id().to_owned();
            move |event: SyncTypingEvent| async move {
                // Ignore typing notifications from own user.
                let typing_user_ids = event
                    .content
                    .user_ids
                    .into_iter()
                    .filter(|user_id| *user_id != own_user_id)
                    .collect();
                // Ignore the result. It can only fail if there are no listeners.
                let _ = sender.send(typing_user_ids);
            }
        });
        let drop_guard = self.client().event_handler_drop_guard(typing_event_handler_handle);
        (drop_guard, receiver)
    }

    /// Subscribe to updates about users who are in "pin violation" i.e. their
    /// identity has changed and the user has not yet acknowledged this.
    ///
    /// The returned receiver will receive a new vector of
    /// [`IdentityStatusChange`] each time a /keys/query response shows a
    /// changed identity for a member of this room, or a sync shows a change
    /// to the membership of an affected user. (Changes to the current user are
    /// not directly included, but some changes to the current user's identity
    /// can trigger changes to how we see other users' identities, which
    /// will be included.)
    ///
    /// The first item in the stream provides the current state of the room:
    /// each member of the room who is not in "pinned" or "verified" state will
    /// be included (except the current user).
    ///
    /// If the `changed_to` property of an [`IdentityStatusChange`] is set to
    /// `PinViolation` then a warning should be displayed to the user. If it is
    /// set to `Pinned` then no warning should be displayed.
    ///
    /// Note that if a user who is in pin violation leaves the room, a `Pinned`
    /// update is sent, to indicate that the warning should be removed, even
    /// though the user's identity is not necessarily pinned.
    #[cfg(feature = "e2e-encryption")]
    pub async fn subscribe_to_identity_status_changes(
        &self,
    ) -> Result<impl Stream<Item = Vec<IdentityStatusChange>> + use<>> {
        IdentityStatusChanges::create_stream(self.clone()).await
    }

    /// Returns a wrapping `TimelineEvent` for the input `AnyTimelineEvent`,
    /// decrypted if needs be.
    ///
    /// Only logs from the crypto crate will indicate a failure to decrypt.
    #[cfg(not(feature = "experimental-encrypted-state-events"))]
    #[allow(clippy::unused_async)] // Used only in e2e-encryption.
    async fn try_decrypt_event(
        &self,
        event: Raw<AnyTimelineEvent>,
        push_ctx: Option<&PushContext>,
    ) -> TimelineEvent {
        #[cfg(feature = "e2e-encryption")]
        if let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(
            SyncMessageLikeEvent::Original(_),
        ))) = event.deserialize_as::<AnySyncTimelineEvent>()
            && let Ok(event) = self.decrypt_event(event.cast_ref_unchecked(), push_ctx).await
        {
            return event;
        }

        let mut event = TimelineEvent::from_plaintext(event.cast());
        if let Some(push_ctx) = push_ctx {
            event.set_push_actions(push_ctx.for_event(event.raw()).await);
        }

        event
    }

    /// Returns a wrapping `TimelineEvent` for the input `AnyTimelineEvent`,
    /// decrypted if needs be.
    ///
    /// Only logs from the crypto crate will indicate a failure to decrypt.
    #[cfg(feature = "experimental-encrypted-state-events")]
    #[allow(clippy::unused_async)] // Used only in e2e-encryption.
    async fn try_decrypt_event(
        &self,
        event: Raw<AnyTimelineEvent>,
        push_ctx: Option<&PushContext>,
    ) -> TimelineEvent {
        // If we have either an encrypted message-like or state event, try to decrypt.
        match event.deserialize_as::<AnySyncTimelineEvent>() {
            Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(
                SyncMessageLikeEvent::Original(_),
            ))) => {
                if let Ok(event) = self
                    .decrypt_event(
                        event.cast_ref_unchecked::<OriginalSyncRoomEncryptedEvent>(),
                        push_ctx,
                    )
                    .await
                {
                    return event;
                }
            }
            Ok(AnySyncTimelineEvent::State(AnySyncStateEvent::RoomEncrypted(
                SyncStateEvent::Original(_),
            ))) => {
                if let Ok(event) = self
                    .decrypt_event(
                        event.cast_ref_unchecked::<OriginalSyncStateRoomEncryptedEvent>(),
                        push_ctx,
                    )
                    .await
                {
                    return event;
                }
            }
            _ => {}
        }

        let mut event = TimelineEvent::from_plaintext(event.cast());
        if let Some(push_ctx) = push_ctx {
            event.set_push_actions(push_ctx.for_event(event.raw()).await);
        }

        event
    }

    /// Fetch the event with the given `EventId` in this room.
    ///
    /// It uses the given [`RequestConfig`] if provided, or the client's default
    /// one otherwise.
    pub async fn event(
        &self,
        event_id: &EventId,
        request_config: Option<RequestConfig>,
    ) -> Result<TimelineEvent> {
        let request =
            get_room_event::v3::Request::new(self.room_id().to_owned(), event_id.to_owned());

        let raw_event = self.client.send(request).with_request_config(request_config).await?.event;
        let push_ctx = self.push_context().await?;
        let event = self.try_decrypt_event(raw_event, push_ctx.as_ref()).await;

        // Save the event into the event cache, if it's set up.
        if let Ok((cache, _handles)) = self.event_cache().await {
            cache.save_events([event.clone()]).await;
        }

        Ok(event)
    }

    /// Try to load the event from the [`EventCache`][crate::event_cache], if
    /// it's enabled, or fetch it from the homeserver.
    ///
    /// When running the request against the homeserver, it uses the given
    /// [`RequestConfig`] if provided, or the client's default one
    /// otherwise.
    pub async fn load_or_fetch_event(
        &self,
        event_id: &EventId,
        request_config: Option<RequestConfig>,
    ) -> Result<TimelineEvent> {
        match self.event_cache().await {
            Ok((event_cache, _drop_handles)) => {
                if let Some(event) = event_cache.find_event(event_id).await? {
                    return Ok(event);
                }
                // Fallthrough: try with a request.
            }
            Err(err) => {
                debug!("error when getting the event cache: {err}");
            }
        }
        self.event(event_id, request_config).await
    }

    /// Fetch the event with the given `EventId` in this room, using the
    /// `/context` endpoint to get more information.
    pub async fn event_with_context(
        &self,
        event_id: &EventId,
        lazy_load_members: bool,
        context_size: UInt,
        request_config: Option<RequestConfig>,
    ) -> Result<EventWithContextResponse> {
        let mut request =
            context::get_context::v3::Request::new(self.room_id().to_owned(), event_id.to_owned());

        request.limit = context_size;

        if lazy_load_members {
            request.filter.lazy_load_options =
                LazyLoadOptions::Enabled { include_redundant_members: false };
        }

        let response = self.client.send(request).with_request_config(request_config).await?;

        let push_ctx = self.push_context().await?;
        let push_ctx = push_ctx.as_ref();
        let target_event = if let Some(event) = response.event {
            Some(self.try_decrypt_event(event, push_ctx).await)
        } else {
            None
        };

        // Note: the joined future will fail if any future failed, but
        // [`Self::try_decrypt_event`] doesn't hard-fail when there's a
        // decryption error, so we should prevent against most bad cases here.
        let (events_before, events_after) = join!(
            join_all(
                response.events_before.into_iter().map(|ev| self.try_decrypt_event(ev, push_ctx)),
            ),
            join_all(
                response.events_after.into_iter().map(|ev| self.try_decrypt_event(ev, push_ctx)),
            ),
        );

        // Save the loaded events into the event cache, if it's set up.
        if let Ok((cache, _handles)) = self.event_cache().await {
            let mut events_to_save: Vec<TimelineEvent> = Vec::new();
            if let Some(event) = &target_event {
                events_to_save.push(event.clone());
            }

            for event in &events_before {
                events_to_save.push(event.clone());
            }

            for event in &events_after {
                events_to_save.push(event.clone());
            }

            cache.save_events(events_to_save).await;
        }

        Ok(EventWithContextResponse {
            event: target_event,
            events_before,
            events_after,
            state: response.state,
            prev_batch_token: response.start,
            next_batch_token: response.end,
        })
    }

    pub(crate) async fn request_members(&self) -> Result<()> {
        self.client
            .locks()
            .members_request_deduplicated_handler
            .run(self.room_id().to_owned(), async move {
                let request = get_member_events::v3::Request::new(self.inner.room_id().to_owned());
                let response = self
                    .client
                    .send(request.clone())
                    .with_request_config(
                        // In some cases it can take longer than 30s to load:
                        // https://github.com/element-hq/synapse/issues/16872
                        RequestConfig::new().timeout(Duration::from_secs(60)).retry_limit(3),
                    )
                    .await?;

                // That's a large `Future`. Let's `Box::pin` to reduce its size on the stack.
                Box::pin(self.client.base_client().receive_all_members(
                    self.room_id(),
                    &request,
                    &response,
                ))
                .await?;

                Ok(())
            })
            .await
    }

    /// Request to update the encryption state for this room.
    ///
    /// It does nothing if the encryption state is already
    /// [`EncryptionState::Encrypted`] or [`EncryptionState::NotEncrypted`].
    pub async fn request_encryption_state(&self) -> Result<()> {
        if !self.inner.encryption_state().is_unknown() {
            return Ok(());
        }

        self.client
            .locks()
            .encryption_state_deduplicated_handler
            .run(self.room_id().to_owned(), async move {
                // Request the event from the server.
                let request = get_state_event_for_key::v3::Request::new(
                    self.room_id().to_owned(),
                    StateEventType::RoomEncryption,
                    "".to_owned(),
                );
                let response = match self.client.send(request).await {
                    Ok(response) => Some(
                        response
                            .into_content()
                            .deserialize_as_unchecked::<RoomEncryptionEventContent>()?,
                    ),
                    Err(err) if err.client_api_error_kind() == Some(&ErrorKind::NotFound) => None,
                    Err(err) => return Err(err.into()),
                };

                let _state_store_lock = self.client.base_client().state_store_lock().lock().await;

                // Persist the event and the fact that we requested it from the server in
                // `RoomInfo`.
                let mut room_info = self.clone_info();
                room_info.mark_encryption_state_synced();
                room_info.set_encryption_event(response.clone());
                let mut changes = StateChanges::default();
                changes.add_room(room_info.clone());

                self.client.state_store().save_changes(&changes).await?;
                self.set_room_info(room_info, RoomInfoNotableUpdateReasons::empty());

                Ok(())
            })
            .await
    }

    /// Check the encryption state of this room.
    ///
    /// If the result is [`EncryptionState::Unknown`], one might want to call
    /// [`Room::request_encryption_state`].
    pub fn encryption_state(&self) -> EncryptionState {
        self.inner.encryption_state()
    }

    /// Force to update the encryption state by calling
    /// [`Room::request_encryption_state`], and then calling
    /// [`Room::encryption_state`].
    ///
    /// This method is useful to ensure the encryption state is up-to-date.
    pub async fn latest_encryption_state(&self) -> Result<EncryptionState> {
        self.request_encryption_state().await?;

        Ok(self.encryption_state())
    }

    /// Gets additional context info about the client crypto.
    #[cfg(feature = "e2e-encryption")]
    pub async fn crypto_context_info(&self) -> CryptoContextInfo {
        let encryption = self.client.encryption();

        let this_device_is_verified = match encryption.get_own_device().await {
            Ok(Some(device)) => device.is_verified_with_cross_signing(),

            // Should not happen, there will always be an own device
            _ => true,
        };

        let backup_exists_on_server =
            encryption.backups().exists_on_server().await.unwrap_or(false);

        CryptoContextInfo {
            device_creation_ts: encryption.device_creation_timestamp().await,
            this_device_is_verified,
            is_backup_configured: encryption.backups().state() == BackupState::Enabled,
            backup_exists_on_server,
        }
    }

    fn are_events_visible(&self) -> bool {
        if let RoomState::Invited = self.inner.state() {
            return matches!(
                self.inner.history_visibility_or_default(),
                HistoryVisibility::WorldReadable | HistoryVisibility::Invited
            );
        }

        true
    }

    /// Sync the member list with the server.
    ///
    /// This method will de-duplicate requests if it is called multiple times in
    /// quick succession, in that case the return value will be `None`. This
    /// method does nothing if the members are already synced.
    pub async fn sync_members(&self) -> Result<()> {
        if !self.are_events_visible() {
            return Ok(());
        }

        if !self.are_members_synced() { self.request_members().await } else { Ok(()) }
    }

    /// Get a specific member of this room.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that it might panic if it isn't run on a tokio thread.
    ///
    /// Use [get_member_no_sync()](#method.get_member_no_sync) if you want a
    /// method that doesn't do any requests.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user that should be fetched out of the
    ///   store.
    pub async fn get_member(&self, user_id: &UserId) -> Result<Option<RoomMember>> {
        self.sync_members().await?;
        self.get_member_no_sync(user_id).await
    }

    /// Get a specific member of this room.
    ///
    /// *Note*: This method will not fetch the members from the homeserver if
    /// the member list isn't synchronized due to member lazy loading. Thus,
    /// members could be missing.
    ///
    /// Use [get_member()](#method.get_member) if you want to ensure to always
    /// have the full member list to chose from.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user that should be fetched out of the
    ///   store.
    pub async fn get_member_no_sync(&self, user_id: &UserId) -> Result<Option<RoomMember>> {
        Ok(self
            .inner
            .get_member(user_id)
            .await?
            .map(|member| RoomMember::new(self.client.clone(), member)))
    }

    /// Get members for this room, with the given memberships.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that it might panic if it isn't run on a tokio thread.
    ///
    /// Use [members_no_sync()](#method.members_no_sync) if you want a
    /// method that doesn't do any requests.
    pub async fn members(&self, memberships: RoomMemberships) -> Result<Vec<RoomMember>> {
        self.sync_members().await?;
        self.members_no_sync(memberships).await
    }

    /// Get members for this room, with the given memberships.
    ///
    /// *Note*: This method will not fetch the members from the homeserver if
    /// the member list isn't synchronized due to member lazy loading. Thus,
    /// members could be missing.
    ///
    /// Use [members()](#method.members) if you want to ensure to always get
    /// the full member list.
    pub async fn members_no_sync(&self, memberships: RoomMemberships) -> Result<Vec<RoomMember>> {
        Ok(self
            .inner
            .members(memberships)
            .await?
            .into_iter()
            .map(|member| RoomMember::new(self.client.clone(), member))
            .collect())
    }

    /// Sets the display name of the current user within this room.
    ///
    /// *Note*: This is different to [`crate::Account::set_display_name`] which
    /// updates the user's display name across all of their rooms.
    pub async fn set_own_member_display_name(
        &self,
        display_name: Option<String>,
    ) -> Result<send_state_event::v3::Response> {
        let user_id = self.own_user_id();
        let member_event =
            self.get_state_event_static_for_key::<RoomMemberEventContent, _>(user_id).await?;

        let Some(RawSyncOrStrippedState::Sync(raw_event)) = member_event else {
            return Err(Error::InsufficientData);
        };

        let event = raw_event.deserialize()?;

        let mut content = match event {
            SyncStateEvent::Original(original_event) => original_event.content,
            SyncStateEvent::Redacted(redacted_event) => {
                RoomMemberEventContent::new(redacted_event.content.membership)
            }
        };

        content.displayname = display_name;
        self.send_state_event_for_key(user_id, content).await
    }

    /// Get all state events of a given type in this room.
    pub async fn get_state_events(
        &self,
        event_type: StateEventType,
    ) -> Result<Vec<RawAnySyncOrStrippedState>> {
        self.client
            .state_store()
            .get_state_events(self.room_id(), event_type)
            .await
            .map_err(Into::into)
    }

    /// Get all state events of a given statically-known type in this room.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::{
    ///     events::room::member::RoomMemberEventContent, serde::Raw,
    /// };
    ///
    /// let room_members =
    ///     room.get_state_events_static::<RoomMemberEventContent>().await?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn get_state_events_static<C>(&self) -> Result<Vec<RawSyncOrStrippedState<C>>>
    where
        C: StaticEventContent<IsPrefix = ruma::events::False>
            + StaticStateEventContent
            + RedactContent,
        C::Redacted: RedactedStateEventContent,
    {
        Ok(self.client.state_store().get_state_events_static(self.room_id()).await?)
    }

    /// Get the state events of a given type with the given state keys in this
    /// room.
    pub async fn get_state_events_for_keys(
        &self,
        event_type: StateEventType,
        state_keys: &[&str],
    ) -> Result<Vec<RawAnySyncOrStrippedState>> {
        self.client
            .state_store()
            .get_state_events_for_keys(self.room_id(), event_type, state_keys)
            .await
            .map_err(Into::into)
    }

    /// Get the state events of a given statically-known type with the given
    /// state keys in this room.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::Room = todo!();
    /// # let user_ids: &[matrix_sdk::ruma::OwnedUserId] = &[];
    /// use matrix_sdk::ruma::events::room::member::RoomMemberEventContent;
    ///
    /// let room_members = room
    ///     .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(
    ///         user_ids,
    ///     )
    ///     .await?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn get_state_events_for_keys_static<'a, C, K, I>(
        &self,
        state_keys: I,
    ) -> Result<Vec<RawSyncOrStrippedState<C>>>
    where
        C: StaticEventContent<IsPrefix = ruma::events::False>
            + StaticStateEventContent
            + RedactContent,
        C::StateKey: Borrow<K>,
        C::Redacted: RedactedStateEventContent,
        K: AsRef<str> + Sized + Sync + 'a,
        I: IntoIterator<Item = &'a K> + Send,
        I::IntoIter: Send,
    {
        Ok(self
            .client
            .state_store()
            .get_state_events_for_keys_static(self.room_id(), state_keys)
            .await?)
    }

    /// Get a specific state event in this room.
    pub async fn get_state_event(
        &self,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<RawAnySyncOrStrippedState>> {
        self.client
            .state_store()
            .get_state_event(self.room_id(), event_type, state_key)
            .await
            .map_err(Into::into)
    }

    /// Get a specific state event of statically-known type with an empty state
    /// key in this room.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::events::room::power_levels::RoomPowerLevelsEventContent;
    ///
    /// let power_levels = room
    ///     .get_state_event_static::<RoomPowerLevelsEventContent>()
    ///     .await?
    ///     .expect("every room has a power_levels event")
    ///     .deserialize()?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn get_state_event_static<C>(&self) -> Result<Option<RawSyncOrStrippedState<C>>>
    where
        C: StaticEventContent<IsPrefix = ruma::events::False>
            + StaticStateEventContent<StateKey = EmptyStateKey>
            + RedactContent,
        C::Redacted: RedactedStateEventContent,
    {
        self.get_state_event_static_for_key(&EmptyStateKey).await
    }

    /// Get a specific state event of statically-known type in this room.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::{
    ///     events::room::member::RoomMemberEventContent, serde::Raw, user_id,
    /// };
    ///
    /// let member_event = room
    ///     .get_state_event_static_for_key::<RoomMemberEventContent, _>(user_id!(
    ///         "@alice:example.org"
    ///     ))
    ///     .await?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn get_state_event_static_for_key<C, K>(
        &self,
        state_key: &K,
    ) -> Result<Option<RawSyncOrStrippedState<C>>>
    where
        C: StaticEventContent<IsPrefix = ruma::events::False>
            + StaticStateEventContent
            + RedactContent,
        C::StateKey: Borrow<K>,
        C::Redacted: RedactedStateEventContent,
        K: AsRef<str> + ?Sized + Sync,
    {
        Ok(self
            .client
            .state_store()
            .get_state_event_static_for_key(self.room_id(), state_key)
            .await?)
    }

    /// Returns the parents this room advertises as its parents.
    ///
    /// Results are in no particular order.
    pub async fn parent_spaces(&self) -> Result<impl Stream<Item = Result<ParentSpace>> + '_> {
        // Implements this algorithm:
        // https://spec.matrix.org/v1.8/client-server-api/#mspaceparent-relationships

        // Get all m.space.parent events for this room
        Ok(self
            .get_state_events_static::<SpaceParentEventContent>()
            .await?
            .into_iter()
            // Extract state key (ie. the parent's id) and sender
            .filter_map(|parent_event| match parent_event.deserialize() {
                Ok(SyncOrStrippedState::Sync(SyncStateEvent::Original(e))) => {
                    Some((e.state_key.to_owned(), e.sender))
                }
                Ok(SyncOrStrippedState::Sync(SyncStateEvent::Redacted(_))) => None,
                Ok(SyncOrStrippedState::Stripped(e)) => Some((e.state_key.to_owned(), e.sender)),
                Err(e) => {
                    info!(room_id = ?self.room_id(), "Could not deserialize m.space.parent: {e}");
                    None
                }
            })
            // Check whether the parent recognizes this room as its child
            .map(|(state_key, sender): (OwnedRoomId, OwnedUserId)| async move {
                let Some(parent_room) = self.client.get_room(&state_key) else {
                    // We are not in the room, cannot check if the relationship is reciprocal
                    // TODO: try peeking into the room
                    return Ok(ParentSpace::Unverifiable(state_key));
                };
                // Get the m.space.child state of the parent with this room's id
                // as state key.
                if let Some(child_event) = parent_room
                    .get_state_event_static_for_key::<SpaceChildEventContent, _>(self.room_id())
                    .await?
                {
                    match child_event.deserialize() {
                        Ok(SyncOrStrippedState::Sync(SyncStateEvent::Original(_))) => {
                            // There is a valid m.space.child in the parent pointing to
                            // this room
                            return Ok(ParentSpace::Reciprocal(parent_room));
                        }
                        Ok(SyncOrStrippedState::Sync(SyncStateEvent::Redacted(_))) => {}
                        Ok(SyncOrStrippedState::Stripped(_)) => {}
                        Err(e) => {
                            info!(
                                room_id = ?self.room_id(), parent_room_id = ?state_key,
                                "Could not deserialize m.space.child: {e}"
                            );
                        }
                    }
                    // Otherwise the event is either invalid or redacted. If
                    // redacted it would be missing the
                    // `via` key, thereby invalidating that end of the
                    // relationship: https://spec.matrix.org/v1.8/client-server-api/#mspacechild
                }

                // No reciprocal m.space.child found, let's check if the sender has the
                // power to set it
                let Some(member) = parent_room.get_member(&sender).await? else {
                    // Sender is not even in the parent room
                    return Ok(ParentSpace::Illegitimate(parent_room));
                };

                if member.can_send_state(StateEventType::SpaceChild) {
                    // Sender does have the power to set m.room.child
                    Ok(ParentSpace::WithPowerlevel(parent_room))
                } else {
                    Ok(ParentSpace::Illegitimate(parent_room))
                }
            })
            .collect::<FuturesUnordered<_>>())
    }

    /// Read account data in this room, from storage.
    pub async fn account_data(
        &self,
        data_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.client
            .state_store()
            .get_room_account_data_event(self.room_id(), data_type)
            .await
            .map_err(Into::into)
    }

    /// Get account data of a statically-known type in this room, from storage.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::events::fully_read::FullyReadEventContent;
    ///
    /// match room.account_data_static::<FullyReadEventContent>().await? {
    ///     Some(fully_read) => {
    ///         println!("Found read marker: {:?}", fully_read.deserialize()?)
    ///     }
    ///     None => println!("No read marker for this room"),
    /// }
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn account_data_static<C>(&self) -> Result<Option<Raw<RoomAccountDataEvent<C>>>>
    where
        C: StaticEventContent<IsPrefix = ruma::events::False> + RoomAccountDataEventContent,
    {
        Ok(self.account_data(C::TYPE.into()).await?.map(Raw::cast_unchecked))
    }

    /// Check if all members of this room are verified and all their devices are
    /// verified.
    ///
    /// Returns true if all devices in the room are verified, otherwise false.
    #[cfg(feature = "e2e-encryption")]
    pub async fn contains_only_verified_devices(&self) -> Result<bool> {
        let user_ids = self
            .client
            .state_store()
            .get_user_ids(self.room_id(), RoomMemberships::empty())
            .await?;

        for user_id in user_ids {
            let devices = self.client.encryption().get_user_devices(&user_id).await?;
            let any_unverified = devices.devices().any(|d| !d.is_verified());

            if any_unverified {
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Set the given account data event for this room.
    ///
    /// # Example
    /// ```
    /// # async {
    /// # let room: matrix_sdk::Room = todo!();
    /// # let event_id: ruma::OwnedEventId = todo!();
    /// use matrix_sdk::ruma::events::fully_read::FullyReadEventContent;
    /// let content = FullyReadEventContent::new(event_id);
    ///
    /// room.set_account_data(content).await?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn set_account_data<T>(
        &self,
        content: T,
    ) -> Result<set_room_account_data::v3::Response>
    where
        T: RoomAccountDataEventContent,
    {
        let own_user = self.client.user_id().ok_or(Error::AuthenticationRequired)?;

        let request = set_room_account_data::v3::Request::new(
            own_user.to_owned(),
            self.room_id().to_owned(),
            &content,
        )?;

        Ok(self.client.send(request).await?)
    }

    /// Set the given raw account data event in this room.
    ///
    /// # Example
    /// ```
    /// # async {
    /// # let room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::{
    ///     events::{
    ///         AnyRoomAccountDataEventContent, RoomAccountDataEventContent,
    ///         marked_unread::MarkedUnreadEventContent,
    ///     },
    ///     serde::Raw,
    /// };
    /// let marked_unread_content = MarkedUnreadEventContent::new(true);
    /// let full_event: AnyRoomAccountDataEventContent =
    ///     marked_unread_content.clone().into();
    /// room.set_account_data_raw(
    ///     marked_unread_content.event_type(),
    ///     Raw::new(&full_event).unwrap(),
    /// )
    /// .await?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn set_account_data_raw(
        &self,
        event_type: RoomAccountDataEventType,
        content: Raw<AnyRoomAccountDataEventContent>,
    ) -> Result<set_room_account_data::v3::Response> {
        let own_user = self.client.user_id().ok_or(Error::AuthenticationRequired)?;

        let request = set_room_account_data::v3::Request::new_raw(
            own_user.to_owned(),
            self.room_id().to_owned(),
            event_type,
            content,
        );

        Ok(self.client.send(request).await?)
    }

    /// Adds a tag to the room, or updates it if it already exists.
    ///
    /// Returns the [`create_tag::v3::Response`] from the server.
    ///
    /// # Arguments
    /// * `tag` - The tag to add or update.
    ///
    /// * `tag_info` - Information about the tag, generally containing the
    ///   `order` parameter.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::str::FromStr;
    /// # use ruma::events::tag::{TagInfo, TagName, UserTagName};
    /// # async {
    /// # let homeserver = url::Url::parse("http://localhost:8080")?;
    /// # let mut client = matrix_sdk::Client::new(homeserver).await?;
    /// # let room_id = matrix_sdk::ruma::room_id!("!test:localhost");
    /// use matrix_sdk::ruma::events::tag::TagInfo;
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     let mut tag_info = TagInfo::new();
    ///     tag_info.order = Some(0.9);
    ///     let user_tag = UserTagName::from_str("u.work")?;
    ///
    ///     room.set_tag(TagName::User(user_tag), tag_info).await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn set_tag(
        &self,
        tag: TagName,
        tag_info: TagInfo,
    ) -> Result<create_tag::v3::Response> {
        let user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?;
        let request = create_tag::v3::Request::new(
            user_id.to_owned(),
            self.inner.room_id().to_owned(),
            tag.to_string(),
            tag_info,
        );
        Ok(self.client.send(request).await?)
    }

    /// Removes a tag from the room.
    ///
    /// Returns the [`delete_tag::v3::Response`] from the server.
    ///
    /// # Arguments
    /// * `tag` - The tag to remove.
    pub async fn remove_tag(&self, tag: TagName) -> Result<delete_tag::v3::Response> {
        let user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?;
        let request = delete_tag::v3::Request::new(
            user_id.to_owned(),
            self.inner.room_id().to_owned(),
            tag.to_string(),
        );
        Ok(self.client.send(request).await?)
    }

    /// Add or remove the `m.favourite` flag for this room.
    ///
    /// If `is_favourite` is `true`, and the `m.low_priority` tag is set on the
    /// room, the tag will be removed too.
    ///
    /// # Arguments
    ///
    /// * `is_favourite` - Whether to mark this room as favourite.
    /// * `tag_order` - The order of the tag if any.
    pub async fn set_is_favourite(&self, is_favourite: bool, tag_order: Option<f64>) -> Result<()> {
        if is_favourite {
            let tag_info = assign!(TagInfo::new(), { order: tag_order });

            self.set_tag(TagName::Favorite, tag_info).await?;

            if self.is_low_priority() {
                self.remove_tag(TagName::LowPriority).await?;
            }
        } else {
            self.remove_tag(TagName::Favorite).await?;
        }
        Ok(())
    }

    /// Add or remove the `m.lowpriority` flag for this room.
    ///
    /// If `is_low_priority` is `true`, and the `m.favourite` tag is set on the
    /// room, the tag will be removed too.
    ///
    /// # Arguments
    ///
    /// * `is_low_priority` - Whether to mark this room as low_priority or not.
    /// * `tag_order` - The order of the tag if any.
    pub async fn set_is_low_priority(
        &self,
        is_low_priority: bool,
        tag_order: Option<f64>,
    ) -> Result<()> {
        if is_low_priority {
            let tag_info = assign!(TagInfo::new(), { order: tag_order });

            self.set_tag(TagName::LowPriority, tag_info).await?;

            if self.is_favourite() {
                self.remove_tag(TagName::Favorite).await?;
            }
        } else {
            self.remove_tag(TagName::LowPriority).await?;
        }
        Ok(())
    }

    /// Sets whether this room is a DM.
    ///
    /// When setting this room as DM, it will be marked as DM for all active
    /// members of the room. When unsetting this room as DM, it will be
    /// unmarked as DM for all users, not just the members.
    ///
    /// # Arguments
    /// * `is_direct` - Whether to mark this room as direct.
    pub async fn set_is_direct(&self, is_direct: bool) -> Result<()> {
        let user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?;

        let mut content = self
            .client
            .account()
            .account_data::<DirectEventContent>()
            .await?
            .map(|c| c.deserialize())
            .transpose()?
            .unwrap_or_default();

        let this_room_id = self.inner.room_id();

        if is_direct {
            let mut room_members = self.members(RoomMemberships::ACTIVE).await?;
            room_members.retain(|member| member.user_id() != self.own_user_id());

            for member in room_members {
                let entry = content.entry(member.user_id().into()).or_default();
                if !entry.iter().any(|room_id| room_id == this_room_id) {
                    entry.push(this_room_id.to_owned());
                }
            }
        } else {
            for (_, list) in content.iter_mut() {
                list.retain(|room_id| *room_id != this_room_id);
            }

            // Remove user ids that don't have any room marked as DM
            content.retain(|_, list| !list.is_empty());
        }

        let request = set_global_account_data::v3::Request::new(user_id.to_owned(), &content)?;

        self.client.send(request).await?;
        Ok(())
    }

    /// Tries to decrypt a room event.
    ///
    /// # Arguments
    /// * `event` - The room event to be decrypted.
    ///
    /// Returns the decrypted event. In the case of a decryption error, returns
    /// a `TimelineEvent` representing the decryption error.
    #[cfg(feature = "e2e-encryption")]
    #[cfg(not(feature = "experimental-encrypted-state-events"))]
    pub async fn decrypt_event(
        &self,
        event: &Raw<OriginalSyncRoomEncryptedEvent>,
        push_ctx: Option<&PushContext>,
    ) -> Result<TimelineEvent> {
        let machine = self.client.olm_machine().await;
        let machine = machine.as_ref().ok_or(Error::NoOlmMachine)?;

        match machine
            .try_decrypt_room_event(
                event.cast_ref(),
                self.inner.room_id(),
                self.client.decryption_settings(),
            )
            .await?
        {
            RoomEventDecryptionResult::Decrypted(decrypted) => {
                let push_actions = if let Some(push_ctx) = push_ctx {
                    Some(push_ctx.for_event(&decrypted.event).await)
                } else {
                    None
                };
                Ok(TimelineEvent::from_decrypted(decrypted, push_actions))
            }
            RoomEventDecryptionResult::UnableToDecrypt(utd_info) => {
                self.client
                    .encryption()
                    .backups()
                    .maybe_download_room_key(self.room_id().to_owned(), event.clone());
                Ok(TimelineEvent::from_utd(event.clone().cast(), utd_info))
            }
        }
    }

    /// Tries to decrypt a room event.
    ///
    /// # Arguments
    /// * `event` - The room event to be decrypted.
    ///
    /// Returns the decrypted event. In the case of a decryption error, returns
    /// a `TimelineEvent` representing the decryption error.
    #[cfg(feature = "experimental-encrypted-state-events")]
    pub async fn decrypt_event<T: JsonCastable<EncryptedEvent>>(
        &self,
        event: &Raw<T>,
        push_ctx: Option<&PushContext>,
    ) -> Result<TimelineEvent> {
        let machine = self.client.olm_machine().await;
        let machine = machine.as_ref().ok_or(Error::NoOlmMachine)?;

        match machine
            .try_decrypt_room_event(
                event.cast_ref(),
                self.inner.room_id(),
                self.client.decryption_settings(),
            )
            .await?
        {
            RoomEventDecryptionResult::Decrypted(decrypted) => {
                let push_actions = if let Some(push_ctx) = push_ctx {
                    Some(push_ctx.for_event(&decrypted.event).await)
                } else {
                    None
                };
                Ok(TimelineEvent::from_decrypted(decrypted, push_actions))
            }
            RoomEventDecryptionResult::UnableToDecrypt(utd_info) => {
                self.client
                    .encryption()
                    .backups()
                    .maybe_download_room_key(self.room_id().to_owned(), event.clone());
                // Cast safety: Anything that can be cast to EncryptedEvent must be a timeline
                // event.
                Ok(TimelineEvent::from_utd(event.clone().cast_unchecked(), utd_info))
            }
        }
    }

    /// Fetches the [`EncryptionInfo`] for an event decrypted with the supplied
    /// session_id.
    ///
    /// This may be used when we receive an update for a session, and we want to
    /// reflect the changes in messages we have received that were encrypted
    /// with that session, e.g. to remove a warning shield because a device is
    /// now verified.
    ///
    /// # Arguments
    /// * `session_id` - The ID of the Megolm session to get information for.
    /// * `sender` - The (claimed) sender of the event where the session was
    ///   used.
    #[cfg(feature = "e2e-encryption")]
    pub async fn get_encryption_info(
        &self,
        session_id: &str,
        sender: &UserId,
    ) -> Option<Arc<EncryptionInfo>> {
        let machine = self.client.olm_machine().await;
        let machine = machine.as_ref()?;
        machine.get_session_encryption_info(self.room_id(), session_id, sender).await.ok()
    }

    /// Forces the currently active room key, which is used to encrypt messages,
    /// to be rotated.
    ///
    /// A new room key will be crated and shared with all the room members the
    /// next time a message will be sent. You don't have to call this method,
    /// room keys will be rotated automatically when necessary. This method is
    /// still useful for debugging purposes.
    ///
    /// For more info please take a look a the [`encryption`] module
    /// documentation.
    ///
    /// [`encryption`]: crate::encryption
    #[cfg(feature = "e2e-encryption")]
    pub async fn discard_room_key(&self) -> Result<()> {
        let machine = self.client.olm_machine().await;
        if let Some(machine) = machine.as_ref() {
            machine.discard_room_key(self.inner.room_id()).await?;
            Ok(())
        } else {
            Err(Error::NoOlmMachine)
        }
    }

    /// Ban the user with `UserId` from this room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user to ban with `UserId`.
    ///
    /// * `reason` - The reason for banning this user.
    #[instrument(skip_all)]
    pub async fn ban_user(&self, user_id: &UserId, reason: Option<&str>) -> Result<()> {
        let request = assign!(
            ban_user::v3::Request::new(self.room_id().to_owned(), user_id.to_owned()),
            { reason: reason.map(ToOwned::to_owned) }
        );
        self.client.send(request).await?;
        Ok(())
    }

    /// Unban the user with `UserId` from this room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user to unban with `UserId`.
    ///
    /// * `reason` - The reason for unbanning this user.
    #[instrument(skip_all)]
    pub async fn unban_user(&self, user_id: &UserId, reason: Option<&str>) -> Result<()> {
        let request = assign!(
            unban_user::v3::Request::new(self.room_id().to_owned(), user_id.to_owned()),
            { reason: reason.map(ToOwned::to_owned) }
        );
        self.client.send(request).await?;
        Ok(())
    }

    /// Kick a user out of this room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The `UserId` of the user that should be kicked out of the
    ///   room.
    ///
    /// * `reason` - Optional reason why the room member is being kicked out.
    #[instrument(skip_all)]
    pub async fn kick_user(&self, user_id: &UserId, reason: Option<&str>) -> Result<()> {
        let request = assign!(
            kick_user::v3::Request::new(self.room_id().to_owned(), user_id.to_owned()),
            { reason: reason.map(ToOwned::to_owned) }
        );
        self.client.send(request).await?;
        Ok(())
    }

    /// Invite the specified user by `UserId` to this room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The `UserId` of the user to invite to the room.
    #[instrument(skip_all)]
    pub async fn invite_user_by_id(&self, user_id: &UserId) -> Result<()> {
        #[cfg(feature = "e2e-encryption")]
        if self.client.inner.enable_share_history_on_invite {
            shared_room_history::share_room_history(self, user_id.to_owned()).await?;
        }

        let recipient = InvitationRecipient::UserId { user_id: user_id.to_owned() };
        let request = invite_user::v3::Request::new(self.room_id().to_owned(), recipient);
        self.client.send(request).await?;

        // Force a future room members reload before sending any event to prevent UTDs
        // that can happen when some event is sent after a room member has been invited
        // but before the /sync request could fetch the membership change event.
        self.mark_members_missing();

        Ok(())
    }

    /// Invite the specified user by third party id to this room.
    ///
    /// # Arguments
    ///
    /// * `invite_id` - A third party id of a user to invite to the room.
    #[instrument(skip_all)]
    pub async fn invite_user_by_3pid(&self, invite_id: Invite3pid) -> Result<()> {
        let recipient = InvitationRecipient::ThirdPartyId(invite_id);
        let request = invite_user::v3::Request::new(self.room_id().to_owned(), recipient);
        self.client.send(request).await?;

        // Force a future room members reload before sending any event to prevent UTDs
        // that can happen when some event is sent after a room member has been invited
        // but before the /sync request could fetch the membership change event.
        self.mark_members_missing();

        Ok(())
    }

    /// Activate typing notice for this room.
    ///
    /// The typing notice remains active for 4s. It can be deactivate at any
    /// point by setting typing to `false`. If this method is called while
    /// the typing notice is active nothing will happen. This method can be
    /// called on every key stroke, since it will do nothing while typing is
    /// active.
    ///
    /// # Arguments
    ///
    /// * `typing` - Whether the user is typing or has stopped typing.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    ///
    /// use matrix_sdk::ruma::api::client::typing::create_typing_event::v3::Typing;
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use url::Url;
    ///
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.typing_notice(true).await?
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn typing_notice(&self, typing: bool) -> Result<()> {
        self.ensure_room_joined()?;

        // Only send a request to the homeserver if the old timeout has elapsed
        // or the typing notice changed state within the `TYPING_NOTICE_TIMEOUT`
        let send = if let Some(typing_time) =
            self.client.inner.typing_notice_times.read().unwrap().get(self.room_id())
        {
            if typing_time.elapsed() > TYPING_NOTICE_RESEND_TIMEOUT {
                // We always reactivate the typing notice if typing is true or
                // we may need to deactivate it if it's
                // currently active if typing is false
                typing || typing_time.elapsed() <= TYPING_NOTICE_TIMEOUT
            } else {
                // Only send a request when we need to deactivate typing
                !typing
            }
        } else {
            // Typing notice is currently deactivated, therefore, send a request
            // only when it's about to be activated
            typing
        };

        if send {
            self.send_typing_notice(typing).await?;
        }

        Ok(())
    }

    #[instrument(name = "typing_notice", skip(self))]
    async fn send_typing_notice(&self, typing: bool) -> Result<()> {
        let typing = if typing {
            self.client
                .inner
                .typing_notice_times
                .write()
                .unwrap()
                .insert(self.room_id().to_owned(), Instant::now());
            Typing::Yes(TYPING_NOTICE_TIMEOUT)
        } else {
            self.client.inner.typing_notice_times.write().unwrap().remove(self.room_id());
            Typing::No
        };

        let request = create_typing_event::v3::Request::new(
            self.own_user_id().to_owned(),
            self.room_id().to_owned(),
            typing,
        );

        self.client.send(request).await?;

        Ok(())
    }

    /// Send a request to set a single receipt.
    ///
    /// If an unthreaded receipt is sent, this will also unset the unread flag
    /// of the room if necessary.
    ///
    /// # Arguments
    ///
    /// * `receipt_type` - The type of the receipt to set. Note that it is
    ///   possible to set the fully-read marker although it is technically not a
    ///   receipt.
    ///
    /// * `thread` - The thread where this receipt should apply, if any. Note
    ///   that this must be [`ReceiptThread::Unthreaded`] when sending a
    ///   [`ReceiptType::FullyRead`][create_receipt::v3::ReceiptType::FullyRead].
    ///
    /// * `event_id` - The `EventId` of the event to set the receipt on.
    #[instrument(skip_all)]
    pub async fn send_single_receipt(
        &self,
        receipt_type: create_receipt::v3::ReceiptType,
        thread: ReceiptThread,
        event_id: OwnedEventId,
    ) -> Result<()> {
        // Since the receipt type and the thread aren't Hash/Ord, flatten then as a
        // string key.
        let request_key = format!("{}|{}", receipt_type, thread.as_str().unwrap_or("<unthreaded>"));

        self.client
            .inner
            .locks
            .read_receipt_deduplicated_handler
            .run((request_key, event_id.clone()), async {
                // We will unset the unread flag if we send an unthreaded receipt.
                let is_unthreaded = thread == ReceiptThread::Unthreaded;

                let mut request = create_receipt::v3::Request::new(
                    self.room_id().to_owned(),
                    receipt_type,
                    event_id,
                );
                request.thread = thread;

                self.client.send(request).await?;

                if is_unthreaded {
                    self.set_unread_flag(false).await?;
                }

                Ok(())
            })
            .await
    }

    /// Send a request to set multiple receipts at once.
    ///
    /// This will also unset the unread flag of the room if necessary.
    ///
    /// # Arguments
    ///
    /// * `receipts` - The `Receipts` to send.
    ///
    /// If `receipts` is empty, this is a no-op.
    #[instrument(skip_all)]
    pub async fn send_multiple_receipts(&self, receipts: Receipts) -> Result<()> {
        if receipts.is_empty() {
            return Ok(());
        }

        let Receipts { fully_read, public_read_receipt, private_read_receipt } = receipts;
        let request = assign!(set_read_marker::v3::Request::new(self.room_id().to_owned()), {
            fully_read,
            read_receipt: public_read_receipt,
            private_read_receipt,
        });

        self.client.send(request).await?;

        self.set_unread_flag(false).await?;

        Ok(())
    }

    /// Helper function to enable End-to-end encryption in this room.
    /// `encrypted_state_events` is not used unless the
    /// `experimental-encrypted-state-events` feature is enabled.
    #[allow(unused_variables, unused_mut)]
    async fn enable_encryption_inner(&self, encrypted_state_events: bool) -> Result<()> {
        use ruma::{
            EventEncryptionAlgorithm, events::room::encryption::RoomEncryptionEventContent,
        };
        const SYNC_WAIT_TIME: Duration = Duration::from_secs(3);

        if !self.latest_encryption_state().await?.is_encrypted() {
            let mut content =
                RoomEncryptionEventContent::new(EventEncryptionAlgorithm::MegolmV1AesSha2);
            #[cfg(feature = "experimental-encrypted-state-events")]
            if encrypted_state_events {
                content = content.with_encrypted_state();
            }
            self.send_state_event(content).await?;

            // Spin on the sync beat event, since the first sync we receive might not
            // include the encryption event.
            //
            // TODO do we want to return an error here if we time out? This
            // could be quite useful if someone wants to enable encryption and
            // send a message right after it's enabled.
            let res = timeout(
                async {
                    loop {
                        // Listen for sync events, then check if the encryption state is known.
                        self.client.inner.sync_beat.listen().await;
                        let _state_store_lock =
                            self.client.base_client().state_store_lock().lock().await;

                        if !self.inner.encryption_state().is_unknown() {
                            break;
                        }
                    }
                },
                SYNC_WAIT_TIME,
            )
            .await;

            let _state_store_lock = self.client.base_client().state_store_lock().lock().await;

            // If encryption was enabled, return.
            #[cfg(not(feature = "experimental-encrypted-state-events"))]
            if res.is_ok() && self.inner.encryption_state().is_encrypted() {
                debug!("room successfully marked as encrypted");
                return Ok(());
            }

            // If encryption with state event encryption was enabled, return.
            #[cfg(feature = "experimental-encrypted-state-events")]
            if res.is_ok() && {
                if encrypted_state_events {
                    self.inner.encryption_state().is_state_encrypted()
                } else {
                    self.inner.encryption_state().is_encrypted()
                }
            } {
                debug!("room successfully marked as encrypted");
                return Ok(());
            }

            // If after waiting for multiple syncs, we don't have the encryption state we
            // expect, assume the local encryption state is incorrect; this will
            // cause the SDK to re-request it later for confirmation, instead of
            // assuming it's sync'd and correct (and not encrypted).
            debug!("still not marked as encrypted, marking encryption state as missing");

            let mut room_info = self.clone_info();
            room_info.mark_encryption_state_missing();
            let mut changes = StateChanges::default();
            changes.add_room(room_info.clone());

            self.client.state_store().save_changes(&changes).await?;
            self.set_room_info(room_info, RoomInfoNotableUpdateReasons::empty());
        }

        Ok(())
    }

    /// Enable End-to-end encryption in this room.
    ///
    /// This method will be a noop if encryption is already enabled, otherwise
    /// sends a `m.room.encryption` state event to the room. This might fail if
    /// you don't have the appropriate power level to enable end-to-end
    /// encryption.
    ///
    /// A sync needs to be received to update the local room state. This method
    /// will wait for a sync to be received, this might time out if no
    /// sync loop is running or if the server is slow.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use url::Url;
    /// #
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.enable_encryption().await?
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all)]
    pub async fn enable_encryption(&self) -> Result<()> {
        self.enable_encryption_inner(false).await
    }

    /// Enable End-to-end encryption in this room, opting into experimental
    /// state event encryption.
    ///
    /// This method will be a noop if encryption is already enabled, otherwise
    /// sends a `m.room.encryption` state event to the room. This might fail if
    /// you don't have the appropriate power level to enable end-to-end
    /// encryption.
    ///
    /// A sync needs to be received to update the local room state. This method
    /// will wait for a sync to be received, this might time out if no
    /// sync loop is running or if the server is slow.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use url::Url;
    /// #
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.enable_encryption_with_state_event_encryption().await?
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all)]
    #[cfg(feature = "experimental-encrypted-state-events")]
    pub async fn enable_encryption_with_state_event_encryption(&self) -> Result<()> {
        self.enable_encryption_inner(true).await
    }

    /// Share a room key with users in the given room.
    ///
    /// This will create Olm sessions with all the users/device pairs in the
    /// room if necessary and share a room key that can be shared with them.
    ///
    /// Does nothing if no room key needs to be shared.
    // TODO: expose this publicly so people can pre-share a group session if
    // e.g. a user starts to type a message for a room.
    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip_all, fields(room_id = ?self.room_id(), store_generation))]
    async fn preshare_room_key(&self) -> Result<()> {
        self.ensure_room_joined()?;

        // Take and release the lock on the store, if needs be.
        let guard = self.client.encryption().spin_lock_store(Some(60000)).await?;
        tracing::Span::current().record("store_generation", guard.map(|guard| guard.generation()));

        self.client
            .locks()
            .group_session_deduplicated_handler
            .run(self.room_id().to_owned(), async move {
                {
                    let members = self
                        .client
                        .state_store()
                        .get_user_ids(self.room_id(), RoomMemberships::ACTIVE)
                        .await?;
                    self.client.claim_one_time_keys(members.iter().map(Deref::deref)).await?;
                };

                let response = self.share_room_key().await;

                // If one of the responses failed invalidate the group
                // session as using it would end up in undecryptable
                // messages.
                if let Err(r) = response {
                    let machine = self.client.olm_machine().await;
                    if let Some(machine) = machine.as_ref() {
                        machine.discard_room_key(self.room_id()).await?;
                    }
                    return Err(r);
                }

                Ok(())
            })
            .await
    }

    /// Share a group session for a room.
    ///
    /// # Panics
    ///
    /// Panics if the client isn't logged in.
    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip_all)]
    async fn share_room_key(&self) -> Result<()> {
        self.ensure_room_joined()?;

        let requests = self.client.base_client().share_room_key(self.room_id()).await?;

        for request in requests {
            let response = self.client.send_to_device(&request).await?;
            self.client.mark_request_as_sent(&request.txn_id, &response).await?;
        }

        Ok(())
    }

    /// Wait for the room to be fully synced.
    ///
    /// This method makes sure the room that was returned when joining a room
    /// has been echoed back in the sync.
    ///
    /// Warning: This waits until a sync happens and does not return if no sync
    /// is happening. It can also return early when the room is not a joined
    /// room anymore.
    #[instrument(skip_all)]
    pub async fn sync_up(&self) {
        while !self.is_synced() && self.state() == RoomState::Joined {
            let wait_for_beat = self.client.inner.sync_beat.listen();
            // We don't care whether it's a timeout or a sync beat.
            let _ = timeout(wait_for_beat, Duration::from_millis(1000)).await;
        }
    }

    /// Send a message-like event to this room.
    ///
    /// Returns the parsed response from the server.
    ///
    /// If the encryption feature is enabled this method will transparently
    /// encrypt the event if this room is encrypted (except for `m.reaction`
    /// events, which are never encrypted).
    ///
    /// **Note**: If you just want to send an event with custom JSON content to
    /// a room, you can use the [`send_raw()`][Self::send_raw] method for that.
    ///
    /// If you want to set a transaction ID for the event, use
    /// [`.with_transaction_id()`][SendMessageLikeEvent::with_transaction_id]
    /// on the returned value before `.await`ing it.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the message event.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::sync::{Arc, RwLock};
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use url::Url;
    /// # use matrix_sdk::ruma::room_id;
    /// # use serde::{Deserialize, Serialize};
    /// use matrix_sdk::ruma::{
    ///     MilliSecondsSinceUnixEpoch, TransactionId,
    ///     events::{
    ///         macros::EventContent,
    ///         room::message::{RoomMessageEventContent, TextMessageEventContent},
    ///     },
    ///     uint,
    /// };
    ///
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// let content = RoomMessageEventContent::text_plain("Hello world");
    /// let txn_id = TransactionId::new();
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.send(content).with_transaction_id(txn_id).await?;
    /// }
    ///
    /// // Custom events work too:
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(type = "org.shiny_new_2fa.token", kind = MessageLike)]
    /// struct TokenEventContent {
    ///     token: String,
    ///     #[serde(rename = "exp")]
    ///     expires_at: MilliSecondsSinceUnixEpoch,
    /// }
    ///
    /// # fn generate_token() -> String { todo!() }
    /// let content = TokenEventContent {
    ///     token: generate_token(),
    ///     expires_at: {
    ///         let now = MilliSecondsSinceUnixEpoch::now();
    ///         MilliSecondsSinceUnixEpoch(now.0 + uint!(30_000))
    ///     },
    /// };
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.send(content).await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub fn send(&self, content: impl MessageLikeEventContent) -> SendMessageLikeEvent<'_> {
        SendMessageLikeEvent::new(self, content)
    }

    /// Run /keys/query requests for all the non-tracked users, and for users
    /// with an out-of-date device list.
    #[cfg(feature = "e2e-encryption")]
    async fn query_keys_for_untracked_or_dirty_users(&self) -> Result<()> {
        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref().expect("Olm machine wasn't started");

        let members =
            self.client.state_store().get_user_ids(self.room_id(), RoomMemberships::ACTIVE).await?;

        let tracked: HashMap<_, _> = olm
            .store()
            .load_tracked_users()
            .await?
            .into_iter()
            .map(|tracked| (tracked.user_id, tracked.dirty))
            .collect();

        // A member has no unknown devices iff it was tracked *and* the tracking is
        // not considered dirty.
        let members_with_unknown_devices =
            members.iter().filter(|member| tracked.get(*member).is_none_or(|dirty| *dirty));

        let (req_id, request) =
            olm.query_keys_for_users(members_with_unknown_devices.map(|owned| owned.borrow()));

        if !request.device_keys.is_empty() {
            self.client.keys_query(&req_id, request.device_keys).await?;
        }

        Ok(())
    }

    /// Send a message-like event with custom JSON content to this room.
    ///
    /// Returns the parsed response from the server.
    ///
    /// If the encryption feature is enabled this method will transparently
    /// encrypt the event if this room is encrypted (except for `m.reaction`
    /// events, which are never encrypted).
    ///
    /// This method is equivalent to the [`send()`][Self::send] method but
    /// allows sending custom JSON payloads, e.g. constructed using the
    /// [`serde_json::json!()`] macro.
    ///
    /// If you want to set a transaction ID for the event, use
    /// [`.with_transaction_id()`][SendRawMessageLikeEvent::with_transaction_id]
    /// on the returned value before `.await`ing it.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The type of the event.
    ///
    /// * `content` - The content of the event as a raw JSON value. The argument
    ///   type can be `serde_json::Value`, but also other raw JSON types; for
    ///   the full list check the documentation of
    ///   [`IntoRawMessageLikeEventContent`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::sync::{Arc, RwLock};
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use url::Url;
    /// # use matrix_sdk::ruma::room_id;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// use serde_json::json;
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.send_raw("m.room.message", json!({ "body": "Hello world" })).await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all, fields(event_type, room_id = ?self.room_id(), transaction_id, is_room_encrypted, event_id))]
    pub fn send_raw<'a>(
        &'a self,
        event_type: &'a str,
        content: impl IntoRawMessageLikeEventContent,
    ) -> SendRawMessageLikeEvent<'a> {
        // Note: the recorded instrument fields are saved in
        // `SendRawMessageLikeEvent::into_future`.
        SendRawMessageLikeEvent::new(self, event_type, content)
    }

    /// Send an attachment to this room.
    ///
    /// This will upload the given data that the reader produces using the
    /// [`upload()`] method and post an event to the given room.
    /// If the room is encrypted and the encryption feature is enabled the
    /// upload will be encrypted.
    ///
    /// This is a convenience method that calls the
    /// [`upload()`] and afterwards the [`send()`].
    ///
    /// # Arguments
    /// * `filename` - The file name.
    ///
    /// * `content_type` - The type of the media, this will be used as the
    /// content-type header.
    ///
    /// * `reader` - A `Reader` that will be used to fetch the raw bytes of the
    /// media.
    ///
    /// * `config` - Metadata and configuration for the attachment.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::fs;
    /// # use matrix_sdk::{Client, ruma::room_id, attachment::AttachmentConfig};
    /// # use url::Url;
    /// # use mime;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// let mut image = fs::read("/home/example/my-cat.jpg")?;
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.send_attachment(
    ///         "my_favorite_cat.jpg",
    ///         &mime::IMAGE_JPEG,
    ///         image,
    ///         AttachmentConfig::new(),
    ///     ).await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [`upload()`]: crate::Media::upload
    /// [`send()`]: Self::send
    #[instrument(skip_all)]
    pub fn send_attachment<'a>(
        &'a self,
        filename: impl Into<String>,
        content_type: &'a Mime,
        data: Vec<u8>,
        config: AttachmentConfig,
    ) -> SendAttachment<'a> {
        SendAttachment::new(self, filename.into(), content_type, data, config)
    }

    /// Prepare and send an attachment to this room.
    ///
    /// This will upload the given data that the reader produces using the
    /// [`upload()`](#method.upload) method and post an event to the given room.
    /// If the room is encrypted and the encryption feature is enabled the
    /// upload will be encrypted.
    ///
    /// This is a convenience method that calls the
    /// [`Client::upload()`](#Client::method.upload) and afterwards the
    /// [`send()`](#method.send).
    ///
    /// # Arguments
    /// * `filename` - The file name.
    ///
    /// * `content_type` - The type of the media, this will be used as the
    ///   content-type header.
    ///
    /// * `reader` - A `Reader` that will be used to fetch the raw bytes of the
    ///   media.
    ///
    /// * `config` - Metadata and configuration for the attachment.
    ///
    /// * `send_progress` - An observable to transmit forward progress about the
    ///   upload.
    ///
    /// * `store_in_cache` - A boolean defining whether the uploaded media will
    ///   be stored in the cache immediately after a successful upload.
    #[instrument(skip_all)]
    pub(super) async fn prepare_and_send_attachment<'a>(
        &'a self,
        filename: String,
        content_type: &'a Mime,
        data: Vec<u8>,
        mut config: AttachmentConfig,
        send_progress: SharedObservable<TransmissionProgress>,
        store_in_cache: bool,
    ) -> Result<send_message_event::v3::Response> {
        self.ensure_room_joined()?;

        let txn_id = config.txn_id.take();
        let mentions = config.mentions.take();

        let thumbnail = config.thumbnail.take();

        // If necessary, store caching data for the thumbnail ahead of time.
        let thumbnail_cache_info = if store_in_cache {
            thumbnail
                .as_ref()
                .map(|thumbnail| (thumbnail.data.clone(), thumbnail.height, thumbnail.width))
        } else {
            None
        };

        #[cfg(feature = "e2e-encryption")]
        let (media_source, thumbnail) = if self.latest_encryption_state().await?.is_encrypted() {
            self.client
                .upload_encrypted_media_and_thumbnail(&data, thumbnail, send_progress)
                .await?
        } else {
            self.client
                .media()
                .upload_plain_media_and_thumbnail(
                    content_type,
                    // TODO: get rid of this clone; wait for Ruma to use `Bytes` or something
                    // similar.
                    data.clone(),
                    thumbnail,
                    send_progress,
                )
                .await?
        };

        #[cfg(not(feature = "e2e-encryption"))]
        let (media_source, thumbnail) = self
            .client
            .media()
            .upload_plain_media_and_thumbnail(content_type, data.clone(), thumbnail, send_progress)
            .await?;

        if store_in_cache {
            let media_store_lock_guard = self.client.media_store().lock().await?;

            // A failure to cache shouldn't prevent the whole upload from finishing
            // properly, so only log errors during caching.

            debug!("caching the media");
            let request =
                MediaRequestParameters { source: media_source.clone(), format: MediaFormat::File };

            if let Err(err) = media_store_lock_guard
                .add_media_content(&request, data, IgnoreMediaRetentionPolicy::No)
                .await
            {
                warn!("unable to cache the media after uploading it: {err}");
            }

            if let Some(((data, height, width), source)) =
                thumbnail_cache_info.zip(thumbnail.as_ref().map(|tuple| &tuple.0))
            {
                debug!("caching the thumbnail");

                let request = MediaRequestParameters {
                    source: source.clone(),
                    format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(width, height)),
                };

                if let Err(err) = media_store_lock_guard
                    .add_media_content(&request, data, IgnoreMediaRetentionPolicy::No)
                    .await
                {
                    warn!("unable to cache the media after uploading it: {err}");
                }
            }
        }

        let content = self
            .make_media_event(
                Room::make_attachment_type(
                    content_type,
                    filename,
                    media_source,
                    config.caption,
                    config.info,
                    thumbnail,
                ),
                mentions,
                config.reply,
            )
            .await?;

        let mut fut = self.send(content);
        if let Some(txn_id) = txn_id {
            fut = fut.with_transaction_id(txn_id);
        }

        fut.await.map(|result| result.response)
    }

    /// Creates the inner [`MessageType`] for an already-uploaded media file
    /// provided by its source.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn make_attachment_type(
        content_type: &Mime,
        filename: String,
        source: MediaSource,
        caption: Option<TextMessageEventContent>,
        info: Option<AttachmentInfo>,
        thumbnail: Option<(MediaSource, Box<ThumbnailInfo>)>,
    ) -> MessageType {
        make_media_type!(MessageType, content_type, filename, source, caption, info, thumbnail)
    }

    /// Creates the [`RoomMessageEventContent`] based on the message type,
    /// mentions and reply information.
    pub(crate) async fn make_media_event(
        &self,
        msg_type: MessageType,
        mentions: Option<Mentions>,
        reply: Option<Reply>,
    ) -> Result<RoomMessageEventContent> {
        let mut content = RoomMessageEventContent::new(msg_type);
        if let Some(mentions) = mentions {
            content = content.add_mentions(mentions);
        }
        if let Some(reply) = reply {
            // Since we just created the event, there is no relation attached to it. Thus,
            // it is safe to add the reply relation without overriding anything.
            content = self.make_reply_event(content.into(), reply).await?;
        }
        Ok(content)
    }

    /// Creates the inner [`GalleryItemType`] for an already-uploaded media file
    /// provided by its source.
    #[cfg(feature = "unstable-msc4274")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn make_gallery_item_type(
        content_type: &Mime,
        filename: String,
        source: MediaSource,
        caption: Option<TextMessageEventContent>,
        info: Option<AttachmentInfo>,
        thumbnail: Option<(MediaSource, Box<ThumbnailInfo>)>,
    ) -> GalleryItemType {
        make_media_type!(GalleryItemType, content_type, filename, source, caption, info, thumbnail)
    }

    /// Update the power levels of a select set of users of this room.
    ///
    /// Issue a `power_levels` state event request to the server, changing the
    /// given UserId -> Int levels. May fail if the `power_levels` aren't
    /// locally known yet or the server rejects the state event update, e.g.
    /// because of insufficient permissions. Neither permissions to update
    /// nor whether the data might be stale is checked prior to issuing the
    /// request.
    pub async fn update_power_levels(
        &self,
        updates: Vec<(&UserId, Int)>,
    ) -> Result<send_state_event::v3::Response> {
        let mut power_levels = self.power_levels().await?;

        for (user_id, new_level) in updates {
            if new_level == power_levels.users_default {
                power_levels.users.remove(user_id);
            } else {
                power_levels.users.insert(user_id.to_owned(), new_level);
            }
        }

        self.send_state_event(RoomPowerLevelsEventContent::try_from(power_levels)?).await
    }

    /// Applies a set of power level changes to this room.
    ///
    /// Any values that are `None` in the given `RoomPowerLevelChanges` will
    /// remain unchanged.
    pub async fn apply_power_level_changes(&self, changes: RoomPowerLevelChanges) -> Result<()> {
        let mut power_levels = self.power_levels().await?;
        power_levels.apply(changes)?;
        self.send_state_event(RoomPowerLevelsEventContent::try_from(power_levels)?).await?;
        Ok(())
    }

    /// Resets the room's power levels to the default values
    ///
    /// [spec]: https://spec.matrix.org/v1.9/client-server-api/#mroompower_levels
    pub async fn reset_power_levels(&self) -> Result<RoomPowerLevels> {
        let creators = self.creators().unwrap_or_default();
        let rules = self.clone_info().room_version_rules_or_default();

        let default_power_levels =
            RoomPowerLevels::new(RoomPowerLevelsSource::None, &rules.authorization, creators);
        let changes = RoomPowerLevelChanges::from(default_power_levels);
        self.apply_power_level_changes(changes).await?;
        Ok(self.power_levels().await?)
    }

    /// Gets the suggested role for the user with the provided `user_id`.
    ///
    /// This method checks the `RoomPowerLevels` events instead of loading the
    /// member list and looking for the member.
    pub async fn get_suggested_user_role(&self, user_id: &UserId) -> Result<RoomMemberRole> {
        let power_level = self.get_user_power_level(user_id).await?;
        Ok(RoomMemberRole::suggested_role_for_power_level(power_level))
    }

    /// Gets the power level the user with the provided `user_id`.
    ///
    /// This method checks the `RoomPowerLevels` events instead of loading the
    /// member list and looking for the member.
    pub async fn get_user_power_level(&self, user_id: &UserId) -> Result<UserPowerLevel> {
        let event = self.power_levels().await?;
        Ok(event.for_user(user_id))
    }

    /// Gets a map with the `UserId` of users with power levels other than `0`
    /// and this power level.
    pub async fn users_with_power_levels(&self) -> HashMap<OwnedUserId, i64> {
        let power_levels = self.power_levels().await.ok();
        let mut user_power_levels = HashMap::<OwnedUserId, i64>::new();
        if let Some(power_levels) = power_levels {
            for (id, level) in power_levels.users.into_iter() {
                user_power_levels.insert(id, level.into());
            }
        }
        user_power_levels
    }

    /// Sets the name of this room.
    pub async fn set_name(&self, name: String) -> Result<send_state_event::v3::Response> {
        self.send_state_event(RoomNameEventContent::new(name)).await
    }

    /// Sets a new topic for this room.
    pub async fn set_room_topic(&self, topic: &str) -> Result<send_state_event::v3::Response> {
        self.send_state_event(RoomTopicEventContent::new(topic.into())).await
    }

    /// Sets the new avatar url for this room.
    ///
    /// # Arguments
    /// * `avatar_url` - The owned Matrix uri that represents the avatar
    /// * `info` - The optional image info that can be provided for the avatar
    pub async fn set_avatar_url(
        &self,
        url: &MxcUri,
        info: Option<avatar::ImageInfo>,
    ) -> Result<send_state_event::v3::Response> {
        self.ensure_room_joined()?;

        let mut room_avatar_event = RoomAvatarEventContent::new();
        room_avatar_event.url = Some(url.to_owned());
        room_avatar_event.info = info.map(Box::new);

        self.send_state_event(room_avatar_event).await
    }

    /// Removes the avatar from the room
    pub async fn remove_avatar(&self) -> Result<send_state_event::v3::Response> {
        self.send_state_event(RoomAvatarEventContent::new()).await
    }

    /// Uploads a new avatar for this room.
    ///
    /// # Arguments
    /// * `mime` - The mime type describing the data
    /// * `data` - The data representation of the avatar
    /// * `info` - The optional image info provided for the avatar, the blurhash
    ///   and the mimetype will always be updated
    pub async fn upload_avatar(
        &self,
        mime: &Mime,
        data: Vec<u8>,
        info: Option<avatar::ImageInfo>,
    ) -> Result<send_state_event::v3::Response> {
        self.ensure_room_joined()?;

        let upload_response = self.client.media().upload(mime, data, None).await?;
        let mut info = info.unwrap_or_default();
        info.blurhash = upload_response.blurhash;
        info.mimetype = Some(mime.to_string());

        self.set_avatar_url(&upload_response.content_uri, Some(info)).await
    }

    /// Send a state event with an empty state key to the homeserver.
    ///
    /// For state events with a non-empty state key, see
    /// [`send_state_event_for_key`][Self::send_state_event_for_key].
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the state event.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use serde::{Deserialize, Serialize};
    /// # async {
    /// # let joined_room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::{
    ///     EventEncryptionAlgorithm,
    ///     events::{
    ///         EmptyStateKey, macros::EventContent,
    ///         room::encryption::RoomEncryptionEventContent,
    ///     },
    /// };
    ///
    /// let encryption_event_content = RoomEncryptionEventContent::new(
    ///     EventEncryptionAlgorithm::MegolmV1AesSha2,
    /// );
    /// joined_room.send_state_event(encryption_event_content).await?;
    ///
    /// // Custom event:
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(
    ///     type = "org.matrix.msc_9000.xxx",
    ///     kind = State,
    ///     state_key_type = EmptyStateKey,
    /// )]
    /// struct XxxStateEventContent {/* fields... */}
    ///
    /// let content: XxxStateEventContent = todo!();
    /// joined_room.send_state_event(content).await?;
    /// # anyhow::Ok(()) };
    /// ```
    #[cfg(not(feature = "experimental-encrypted-state-events"))]
    #[instrument(skip_all)]
    pub async fn send_state_event(
        &self,
        content: impl StateEventContent<StateKey = EmptyStateKey>,
    ) -> Result<send_state_event::v3::Response> {
        self.send_state_event_for_key(&EmptyStateKey, content).await
    }

    /// Send a state event with an empty state key to the homeserver.
    ///
    /// For state events with a non-empty state key, see
    /// [`send_state_event_for_key`][Self::send_state_event_for_key].
    ///
    /// If the experimental state event encryption feature is enabled, this
    /// method will transparently encrypt the event if this room is
    /// encrypted (except if the event type is considered critical for the room
    /// to function, as outlined in [MSC4362][msc4362]).
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the state event.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use serde::{Deserialize, Serialize};
    /// # async {
    /// # let joined_room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::{
    ///     EventEncryptionAlgorithm,
    ///     events::{
    ///         EmptyStateKey, macros::EventContent,
    ///         room::encryption::RoomEncryptionEventContent,
    ///     },
    /// };
    ///
    /// let encryption_event_content = RoomEncryptionEventContent::new(
    ///     EventEncryptionAlgorithm::MegolmV1AesSha2,
    /// );
    /// joined_room.send_state_event(encryption_event_content).await?;
    ///
    /// // Custom event:
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(
    ///     type = "org.matrix.msc_9000.xxx",
    ///     kind = State,
    ///     state_key_type = EmptyStateKey,
    /// )]
    /// struct XxxStateEventContent {/* fields... */}
    ///
    /// let content: XxxStateEventContent = todo!();
    /// joined_room.send_state_event(content).await?;
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [msc4362]: https://github.com/matrix-org/matrix-spec-proposals/blob/travis/msc/encrypted-state/proposals/4362-encrypted-state.md
    #[cfg(feature = "experimental-encrypted-state-events")]
    #[instrument(skip_all)]
    pub fn send_state_event<'a>(
        &'a self,
        content: impl StateEventContent<StateKey = EmptyStateKey>,
    ) -> SendStateEvent<'a> {
        self.send_state_event_for_key(&EmptyStateKey, content)
    }

    /// Send a state event to the homeserver.
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the state event.
    ///
    /// * `state_key` - A unique key which defines the overwriting semantics for
    ///   this piece of room state.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use serde::{Deserialize, Serialize};
    /// # async {
    /// # let joined_room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::{
    ///     events::{
    ///         macros::EventContent,
    ///         room::member::{RoomMemberEventContent, MembershipState},
    ///     },
    ///     mxc_uri,
    /// };
    ///
    /// let avatar_url = mxc_uri!("mxc://example.org/avatar").to_owned();
    /// let mut content = RoomMemberEventContent::new(MembershipState::Join);
    /// content.avatar_url = Some(avatar_url);
    ///
    /// joined_room.send_state_event_for_key(ruma::user_id!("@foo:bar.com"), content).await?;
    ///
    /// // Custom event:
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(type = "org.matrix.msc_9000.xxx", kind = State, state_key_type = String)]
    /// struct XxxStateEventContent { /* fields... */ }
    ///
    /// let content: XxxStateEventContent = todo!();
    /// joined_room.send_state_event_for_key("foo", content).await?;
    /// # anyhow::Ok(()) };
    /// ```
    #[cfg(not(feature = "experimental-encrypted-state-events"))]
    pub async fn send_state_event_for_key<C, K>(
        &self,
        state_key: &K,
        content: C,
    ) -> Result<send_state_event::v3::Response>
    where
        C: StateEventContent,
        C::StateKey: Borrow<K>,
        K: AsRef<str> + ?Sized,
    {
        self.ensure_room_joined()?;
        let request =
            send_state_event::v3::Request::new(self.room_id().to_owned(), state_key, &content)?;
        let response = self.client.send(request).await?;
        Ok(response)
    }

    /// Send a state event to the homeserver. If state encryption is enabled in
    /// this room, the event will be encrypted.
    ///
    /// If the experimental state event encryption feature is enabled, this
    /// method will transparently encrypt the event if this room is
    /// encrypted (except if the event type is considered critical for the room
    /// to function, as outlined in [MSC4362][msc4362]).
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the state event.
    ///
    /// * `state_key` - A unique key which defines the overwriting semantics for
    ///   this piece of room state.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use serde::{Deserialize, Serialize};
    /// # async {
    /// # let joined_room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::{
    ///     events::{
    ///         macros::EventContent,
    ///         room::member::{RoomMemberEventContent, MembershipState},
    ///     },
    ///     mxc_uri,
    /// };
    ///
    /// let avatar_url = mxc_uri!("mxc://example.org/avatar").to_owned();
    /// let mut content = RoomMemberEventContent::new(MembershipState::Join);
    /// content.avatar_url = Some(avatar_url);
    ///
    /// joined_room.send_state_event_for_key(ruma::user_id!("@foo:bar.com"), content).await?;
    ///
    /// // Custom event:
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(type = "org.matrix.msc_9000.xxx", kind = State, state_key_type = String)]
    /// struct XxxStateEventContent { /* fields... */ }
    ///
    /// let content: XxxStateEventContent = todo!();
    /// joined_room.send_state_event_for_key("foo", content).await?;
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [msc4362]: https://github.com/matrix-org/matrix-spec-proposals/pull/4362
    #[cfg(feature = "experimental-encrypted-state-events")]
    pub fn send_state_event_for_key<'a, C, K>(
        &'a self,
        state_key: &K,
        content: C,
    ) -> SendStateEvent<'a>
    where
        C: StateEventContent,
        C::StateKey: Borrow<K>,
        K: AsRef<str> + ?Sized,
    {
        SendStateEvent::new(self, state_key, content)
    }

    /// Send a raw room state event to the homeserver.
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The type of the event that we're sending out.
    ///
    /// * `state_key` - A unique key which defines the overwriting semantics for
    /// this piece of room state. This value is often a zero-length string.
    ///
    /// * `content` - The content of the event as a raw JSON value. The argument
    ///   type can be `serde_json::Value`, but also other raw JSON types; for
    ///   the full list check the documentation of [`IntoRawStateEventContent`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use serde_json::json;
    ///
    /// # async {
    /// # let homeserver = url::Url::parse("http://localhost:8080")?;
    /// # let mut client = matrix_sdk::Client::new(homeserver).await?;
    /// # let room_id = matrix_sdk::ruma::room_id!("!test:localhost");
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.send_state_event_raw("m.room.member", "", json!({
    ///         "avatar_url": "mxc://example.org/SEsfnsuifSDFSSEF",
    ///         "displayname": "Alice Margatroid",
    ///         "membership": "join",
    ///     })).await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    #[cfg(not(feature = "experimental-encrypted-state-events"))]
    #[instrument(skip_all)]
    pub async fn send_state_event_raw(
        &self,
        event_type: &str,
        state_key: &str,
        content: impl IntoRawStateEventContent,
    ) -> Result<send_state_event::v3::Response> {
        self.ensure_room_joined()?;

        let request = send_state_event::v3::Request::new_raw(
            self.room_id().to_owned(),
            event_type.into(),
            state_key.to_owned(),
            content.into_raw_state_event_content(),
        );

        Ok(self.client.send(request).await?)
    }

    /// Send a raw room state event to the homeserver.
    ///
    /// If the experimental state event encryption feature is enabled, this
    /// method will transparently encrypt the event if this room is
    /// encrypted (except if the event type is considered critical for the room
    /// to function, as outlined in [MSC4362][msc4362]).
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The type of the event that we're sending out.
    ///
    /// * `state_key` - A unique key which defines the overwriting semantics for
    /// this piece of room state. This value is often a zero-length string.
    ///
    /// * `content` - The content of the event as a raw JSON value. The argument
    ///   type can be `serde_json::Value`, but also other raw JSON types; for
    ///   the full list check the documentation of [`IntoRawStateEventContent`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use serde_json::json;
    ///
    /// # async {
    /// # let homeserver = url::Url::parse("http://localhost:8080")?;
    /// # let mut client = matrix_sdk::Client::new(homeserver).await?;
    /// # let room_id = matrix_sdk::ruma::room_id!("!test:localhost");
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.send_state_event_raw("m.room.member", "", json!({
    ///         "avatar_url": "mxc://example.org/SEsfnsuifSDFSSEF",
    ///         "displayname": "Alice Margatroid",
    ///         "membership": "join",
    ///     })).await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [msc4362]: https://github.com/matrix-org/matrix-spec-proposals/pull/4362
    #[cfg(feature = "experimental-encrypted-state-events")]
    #[instrument(skip_all)]
    pub fn send_state_event_raw<'a>(
        &'a self,
        event_type: &'a str,
        state_key: &'a str,
        content: impl IntoRawStateEventContent,
    ) -> SendRawStateEvent<'a> {
        SendRawStateEvent::new(self, event_type, state_key, content)
    }

    /// Strips all information out of an event of the room.
    ///
    /// Returns the [`redact_event::v3::Response`] from the server.
    ///
    /// This cannot be undone. Users may redact their own events, and any user
    /// with a power level greater than or equal to the redact power level of
    /// the room may redact events there.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event to redact
    ///
    /// * `reason` - The reason for the event being redacted.
    ///
    /// * `txn_id` - A unique ID that can be attached to this event as
    /// its transaction ID. If not given one is created for the message.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::ruma::event_id;
    ///
    /// # async {
    /// # let homeserver = url::Url::parse("http://localhost:8080")?;
    /// # let mut client = matrix_sdk::Client::new(homeserver).await?;
    /// # let room_id = matrix_sdk::ruma::room_id!("!test:localhost");
    /// #
    /// if let Some(room) = client.get_room(&room_id) {
    ///     let event_id = event_id!("$xxxxxx:example.org");
    ///     let reason = Some("Indecent material");
    ///     room.redact(&event_id, reason, None).await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all)]
    pub async fn redact(
        &self,
        event_id: &EventId,
        reason: Option<&str>,
        txn_id: Option<OwnedTransactionId>,
    ) -> HttpResult<redact_event::v3::Response> {
        let txn_id = txn_id.unwrap_or_else(TransactionId::new);
        let request = assign!(
            redact_event::v3::Request::new(self.room_id().to_owned(), event_id.to_owned(), txn_id),
            { reason: reason.map(ToOwned::to_owned) }
        );

        self.client.send(request).await
    }

    /// Get a list of servers that should know this room.
    ///
    /// Uses the synced members of the room and the suggested [routing
    /// algorithm] from the Matrix spec.
    ///
    /// Returns at most three servers.
    ///
    /// [routing algorithm]: https://spec.matrix.org/v1.3/appendices/#routing
    pub async fn route(&self) -> Result<Vec<OwnedServerName>> {
        let acl_ev = self
            .get_state_event_static::<RoomServerAclEventContent>()
            .await?
            .and_then(|ev| ev.deserialize().ok());
        let acl = acl_ev.as_ref().and_then(|ev| match ev {
            SyncOrStrippedState::Sync(ev) => ev.as_original().map(|ev| &ev.content),
            SyncOrStrippedState::Stripped(ev) => Some(&ev.content),
        });

        // Filter out server names that:
        // - Are blocked due to server ACLs
        // - Are IP addresses
        let members: Vec<_> = self
            .members_no_sync(RoomMemberships::JOIN)
            .await?
            .into_iter()
            .filter(|member| {
                let server = member.user_id().server_name();
                acl.filter(|acl| !acl.is_allowed(server)).is_none() && !server.is_ip_literal()
            })
            .collect();

        // Get the server of the highest power level user in the room, provided
        // they are at least power level 50.
        let max = members
            .iter()
            .max_by_key(|member| member.power_level())
            .filter(|max| max.power_level() >= int!(50))
            .map(|member| member.user_id().server_name());

        // Sort the servers by population.
        let servers = members
            .iter()
            .map(|member| member.user_id().server_name())
            .filter(|server| max.filter(|max| max == server).is_none())
            .fold(BTreeMap::<_, u32>::new(), |mut servers, server| {
                *servers.entry(server).or_default() += 1;
                servers
            });
        let mut servers: Vec<_> = servers.into_iter().collect();
        servers.sort_unstable_by(|(_, count_a), (_, count_b)| count_b.cmp(count_a));

        Ok(max
            .into_iter()
            .chain(servers.into_iter().map(|(name, _)| name))
            .take(3)
            .map(ToOwned::to_owned)
            .collect())
    }

    /// Get a `matrix.to` permalink to this room.
    ///
    /// If this room has an alias, we use it. Otherwise, we try to use the
    /// synced members in the room for [routing] the room ID.
    ///
    /// [routing]: https://spec.matrix.org/v1.3/appendices/#routing
    pub async fn matrix_to_permalink(&self) -> Result<MatrixToUri> {
        if let Some(alias) = self.canonical_alias().or_else(|| self.alt_aliases().pop()) {
            return Ok(alias.matrix_to_uri());
        }

        let via = self.route().await?;
        Ok(self.room_id().matrix_to_uri_via(via))
    }

    /// Get a `matrix:` permalink to this room.
    ///
    /// If this room has an alias, we use it. Otherwise, we try to use the
    /// synced members in the room for [routing] the room ID.
    ///
    /// # Arguments
    ///
    /// * `join` - Whether the user should join the room.
    ///
    /// [routing]: https://spec.matrix.org/v1.3/appendices/#routing
    pub async fn matrix_permalink(&self, join: bool) -> Result<MatrixUri> {
        if let Some(alias) = self.canonical_alias().or_else(|| self.alt_aliases().pop()) {
            return Ok(alias.matrix_uri(join));
        }

        let via = self.route().await?;
        Ok(self.room_id().matrix_uri_via(via, join))
    }

    /// Get a `matrix.to` permalink to an event in this room.
    ///
    /// We try to use the synced members in the room for [routing] the room ID.
    ///
    /// *Note*: This method does not check if the given event ID is actually
    /// part of this room. It needs to be checked before calling this method
    /// otherwise the permalink won't work.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event.
    ///
    /// [routing]: https://spec.matrix.org/v1.3/appendices/#routing
    pub async fn matrix_to_event_permalink(
        &self,
        event_id: impl Into<OwnedEventId>,
    ) -> Result<MatrixToUri> {
        // Don't use the alias because an event is tied to a room ID, but an
        // alias might point to another room, e.g. after a room upgrade.
        let via = self.route().await?;
        Ok(self.room_id().matrix_to_event_uri_via(event_id, via))
    }

    /// Get a `matrix:` permalink to an event in this room.
    ///
    /// We try to use the synced members in the room for [routing] the room ID.
    ///
    /// *Note*: This method does not check if the given event ID is actually
    /// part of this room. It needs to be checked before calling this method
    /// otherwise the permalink won't work.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event.
    ///
    /// [routing]: https://spec.matrix.org/v1.3/appendices/#routing
    pub async fn matrix_event_permalink(
        &self,
        event_id: impl Into<OwnedEventId>,
    ) -> Result<MatrixUri> {
        // Don't use the alias because an event is tied to a room ID, but an
        // alias might point to another room, e.g. after a room upgrade.
        let via = self.route().await?;
        Ok(self.room_id().matrix_event_uri_via(event_id, via))
    }

    /// Get the latest receipt of a user in this room.
    ///
    /// # Arguments
    ///
    /// * `receipt_type` - The type of receipt to get.
    ///
    /// * `thread` - The thread containing the event of the receipt, if any.
    ///
    /// * `user_id` - The ID of the user.
    ///
    /// Returns the ID of the event on which the receipt applies and the
    /// receipt.
    pub async fn load_user_receipt(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        self.inner.load_user_receipt(receipt_type, thread, user_id).await.map_err(Into::into)
    }

    /// Load the receipts for an event in this room from storage.
    ///
    /// # Arguments
    ///
    /// * `receipt_type` - The type of receipt to get.
    ///
    /// * `thread` - The thread containing the event of the receipt, if any.
    ///
    /// * `event_id` - The ID of the event.
    ///
    /// Returns a list of IDs of users who have sent a receipt for the event and
    /// the corresponding receipts.
    pub async fn load_event_receipts(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        self.inner.load_event_receipts(receipt_type, thread, event_id).await.map_err(Into::into)
    }

    /// Get the push-condition context for this room.
    ///
    /// Returns `None` if some data couldn't be found. This should only happen
    /// in brand new rooms, while we process its state.
    pub async fn push_condition_room_ctx(&self) -> Result<Option<PushConditionRoomCtx>> {
        self.push_condition_room_ctx_internal(self.client.enabled_thread_subscriptions()).await
    }

    /// Get the push-condition context for this room, with a choice to include
    /// thread subscriptions or not, based on the extra
    /// `with_threads_subscriptions` parameter.
    ///
    /// Returns `None` if some data couldn't be found. This should only happen
    /// in brand new rooms, while we process its state.
    pub(crate) async fn push_condition_room_ctx_internal(
        &self,
        with_threads_subscriptions: bool,
    ) -> Result<Option<PushConditionRoomCtx>> {
        let room_id = self.room_id();
        let user_id = self.own_user_id();
        let room_info = self.clone_info();
        let member_count = room_info.active_members_count();

        let user_display_name = if let Some(member) = self.get_member_no_sync(user_id).await? {
            member.name().to_owned()
        } else {
            return Ok(None);
        };

        let power_levels = match self.power_levels().await {
            Ok(power_levels) => Some(power_levels.into()),
            Err(error) => {
                if matches!(room_info.state(), RoomState::Joined) {
                    // It's normal to not have the power levels in a non-joined room, so don't log
                    // the error if the room is not joined
                    error!("Could not compute power levels for push conditions: {error}");
                }
                None
            }
        };

        let mut ctx = assign!(PushConditionRoomCtx::new(
            room_id.to_owned(),
            UInt::new(member_count).unwrap_or(UInt::MAX),
            user_id.to_owned(),
            user_display_name,
        ),
        {
            power_levels,
        });

        if with_threads_subscriptions {
            let this = self.clone();
            ctx = ctx.with_has_thread_subscription_fn(move |event_id: &EventId| {
                let room = this.clone();
                Box::pin(async move {
                    if let Ok(maybe_sub) = room.load_or_fetch_thread_subscription(event_id).await {
                        maybe_sub.is_some()
                    } else {
                        false
                    }
                })
            });
        }

        Ok(Some(ctx))
    }

    /// Retrieves a [`PushContext`] that can be used to compute the push
    /// actions for events.
    pub async fn push_context(&self) -> Result<Option<PushContext>> {
        self.push_context_internal(self.client.enabled_thread_subscriptions()).await
    }

    /// Retrieves a [`PushContext`] that can be used to compute the push actions
    /// for events, with a choice to include thread subscriptions or not,
    /// based on the extra `with_threads_subscriptions` parameter.
    #[instrument(skip(self))]
    pub(crate) async fn push_context_internal(
        &self,
        with_threads_subscriptions: bool,
    ) -> Result<Option<PushContext>> {
        let Some(push_condition_room_ctx) =
            self.push_condition_room_ctx_internal(with_threads_subscriptions).await?
        else {
            debug!("Could not aggregate push context");
            return Ok(None);
        };
        let push_rules = self.client().account().push_rules().await?;
        Ok(Some(PushContext::new(push_condition_room_ctx, push_rules)))
    }

    /// Get the push actions for the given event with the current room state.
    ///
    /// Note that it is possible that no push action is returned because the
    /// current room state does not have all the required state events.
    pub async fn event_push_actions<T>(&self, event: &Raw<T>) -> Result<Option<Vec<Action>>> {
        if let Some(ctx) = self.push_context().await? {
            Ok(Some(ctx.for_event(event).await))
        } else {
            Ok(None)
        }
    }

    /// The membership details of the (latest) invite for the logged-in user in
    /// this room.
    pub async fn invite_details(&self) -> Result<Invite> {
        let state = self.state();

        if state != RoomState::Invited {
            return Err(Error::WrongRoomState(Box::new(WrongRoomState::new("Invited", state))));
        }

        let invitee = self
            .get_member_no_sync(self.own_user_id())
            .await?
            .ok_or_else(|| Error::UnknownError(Box::new(InvitationError::EventMissing)))?;
        let event = invitee.event();
        let inviter_id = event.sender();
        let inviter = self.get_member_no_sync(inviter_id).await?;
        Ok(Invite { invitee, inviter })
    }

    /// Get the membership details for the current user.
    ///
    /// Returns:
    ///     - If the user was present in the room, a
    ///       [`RoomMemberWithSenderInfo`] containing both the user info and the
    ///       member info of the sender of the `m.room.member` event.
    ///     - If the current user is not present, an error.
    pub async fn member_with_sender_info(
        &self,
        user_id: &UserId,
    ) -> Result<RoomMemberWithSenderInfo> {
        let Some(member) = self.get_member_no_sync(user_id).await? else {
            return Err(Error::InsufficientData);
        };

        let sender_member =
            if let Some(member) = self.get_member_no_sync(member.event().sender()).await? {
                // If the sender room member info is already available, return it
                Some(member)
            } else if self.are_members_synced() {
                // The room members are synced and we couldn't find the sender info
                None
            } else if self.sync_members().await.is_ok() {
                // Try getting the sender room member info again after syncing
                self.get_member_no_sync(member.event().sender()).await?
            } else {
                None
            };

        Ok(RoomMemberWithSenderInfo { room_member: member, sender_info: sender_member })
    }

    /// Forget this room.
    ///
    /// This communicates to the homeserver that it should forget the room.
    ///
    /// Only left or banned-from rooms can be forgotten.
    pub async fn forget(&self) -> Result<()> {
        let state = self.state();
        match state {
            RoomState::Joined | RoomState::Invited | RoomState::Knocked => {
                return Err(Error::WrongRoomState(Box::new(WrongRoomState::new(
                    "Left / Banned",
                    state,
                ))));
            }
            RoomState::Left | RoomState::Banned => {}
        }

        let request = forget_room::v3::Request::new(self.inner.room_id().to_owned());
        let _response = self.client.send(request).await?;

        // If it was a DM, remove the room from the `m.direct` global account data.
        if self.inner.direct_targets_length() != 0
            && let Err(e) = self.set_is_direct(false).await
        {
            // It is not important whether we managed to remove the room, it will not have
            // any consequences, so just log the error.
            warn!(room_id = ?self.room_id(), "failed to remove room from m.direct account data: {e}");
        }

        self.client.base_client().forget_room(self.inner.room_id()).await?;

        Ok(())
    }

    fn ensure_room_joined(&self) -> Result<()> {
        let state = self.state();
        if state == RoomState::Joined {
            Ok(())
        } else {
            Err(Error::WrongRoomState(Box::new(WrongRoomState::new("Joined", state))))
        }
    }

    /// Get the notification mode.
    pub async fn notification_mode(&self) -> Option<RoomNotificationMode> {
        if !matches!(self.state(), RoomState::Joined) {
            return None;
        }

        let notification_settings = self.client().notification_settings().await;

        // Get the user-defined mode if available
        let notification_mode =
            notification_settings.get_user_defined_room_notification_mode(self.room_id()).await;

        if notification_mode.is_some() {
            notification_mode
        } else if let Ok(is_encrypted) =
            self.latest_encryption_state().await.map(|state| state.is_encrypted())
        {
            // Otherwise, if encrypted status is available, get the default mode for this
            // type of room.
            // From the point of view of notification settings, a `one-to-one` room is one
            // that involves exactly two people.
            let is_one_to_one = IsOneToOne::from(self.active_members_count() == 2);
            let default_mode = notification_settings
                .get_default_room_notification_mode(IsEncrypted::from(is_encrypted), is_one_to_one)
                .await;
            Some(default_mode)
        } else {
            None
        }
    }

    /// Get the user-defined notification mode.
    ///
    /// The result is cached for fast and non-async call. To read the cached
    /// result, use
    /// [`matrix_sdk_base::Room::cached_user_defined_notification_mode`].
    //
    // Note for maintainers:
    //
    // The fact the result is cached is an important property. If you change that in
    // the future, please review all calls to this method.
    pub async fn user_defined_notification_mode(&self) -> Option<RoomNotificationMode> {
        if !matches!(self.state(), RoomState::Joined) {
            return None;
        }

        let notification_settings = self.client().notification_settings().await;

        // Get the user-defined mode if available.
        let mode =
            notification_settings.get_user_defined_room_notification_mode(self.room_id()).await;

        if let Some(mode) = mode {
            self.update_cached_user_defined_notification_mode(mode);
        }

        mode
    }

    /// Report an event as inappropriate to the homeserver's administrator.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event to report.
    /// * `score` - The score to rate this content.
    /// * `reason` - The reason the content is being reported.
    ///
    /// # Errors
    ///
    /// Returns an error if the room is not joined or if an error occurs with
    /// the request.
    pub async fn report_content(
        &self,
        event_id: OwnedEventId,
        score: Option<ReportedContentScore>,
        reason: Option<String>,
    ) -> Result<report_content::v3::Response> {
        let state = self.state();
        if state != RoomState::Joined {
            return Err(Error::WrongRoomState(Box::new(WrongRoomState::new("Joined", state))));
        }

        let request = report_content::v3::Request::new(
            self.inner.room_id().to_owned(),
            event_id,
            score.map(Into::into),
            reason,
        );
        Ok(self.client.send(request).await?)
    }

    /// Reports a room as inappropriate to the server.
    /// The caller is not required to be joined to the room to report it.
    ///
    /// # Arguments
    ///
    /// * `reason` - The reason the room is being reported.
    ///
    /// # Errors
    ///
    /// Returns an error if the room is not found or on rate limit
    pub async fn report_room(&self, reason: String) -> Result<report_room::v3::Response> {
        let request = report_room::v3::Request::new(self.inner.room_id().to_owned(), reason);

        Ok(self.client.send(request).await?)
    }

    /// Set a flag on the room to indicate that the user has explicitly marked
    /// it as (un)read.
    ///
    /// This is a no-op if [`BaseRoom::is_marked_unread()`] returns the same
    /// value as `unread`.
    pub async fn set_unread_flag(&self, unread: bool) -> Result<()> {
        if self.is_marked_unread() == unread {
            // The request is not necessary.
            return Ok(());
        }

        let user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?;

        let content = MarkedUnreadEventContent::new(unread);

        let request = set_room_account_data::v3::Request::new(
            user_id.to_owned(),
            self.inner.room_id().to_owned(),
            &content,
        )?;

        self.client.send(request).await?;
        Ok(())
    }

    /// Returns the [`RoomEventCache`] associated to this room, assuming the
    /// global [`EventCache`] has been enabled for subscription.
    pub async fn event_cache(
        &self,
    ) -> event_cache::Result<(RoomEventCache, Arc<EventCacheDropHandles>)> {
        self.client.event_cache().for_room(self.room_id()).await
    }

    /// Get the beacon information event in the room for the `user_id`.
    ///
    /// # Errors
    ///
    /// Returns an error if the event is redacted, stripped, not found or could
    /// not be deserialized.
    pub(crate) async fn get_user_beacon_info(
        &self,
        user_id: &UserId,
    ) -> Result<OriginalSyncStateEvent<BeaconInfoEventContent>, BeaconError> {
        let raw_event = self
            .get_state_event_static_for_key::<BeaconInfoEventContent, _>(user_id)
            .await?
            .ok_or(BeaconError::NotFound)?;

        match raw_event.deserialize()? {
            SyncOrStrippedState::Sync(SyncStateEvent::Original(beacon_info)) => Ok(beacon_info),
            SyncOrStrippedState::Sync(SyncStateEvent::Redacted(_)) => Err(BeaconError::Redacted),
            SyncOrStrippedState::Stripped(_) => Err(BeaconError::Stripped),
        }
    }

    /// Start sharing live location in the room.
    ///
    /// # Arguments
    ///
    /// * `duration_millis` - The duration for which the live location is
    ///   shared, in milliseconds.
    /// * `description` - An optional description for the live location share.
    ///
    /// # Errors
    ///
    /// Returns an error if the room is not joined or if the state event could
    /// not be sent.
    pub async fn start_live_location_share(
        &self,
        duration_millis: u64,
        description: Option<String>,
    ) -> Result<send_state_event::v3::Response> {
        self.ensure_room_joined()?;

        self.send_state_event_for_key(
            self.own_user_id(),
            BeaconInfoEventContent::new(
                description,
                Duration::from_millis(duration_millis),
                true,
                None,
            ),
        )
        .await
    }

    /// Stop sharing live location in the room.
    ///
    /// # Errors
    ///
    /// Returns an error if the room is not joined, if the beacon information
    /// is redacted or stripped, or if the state event is not found.
    pub async fn stop_live_location_share(
        &self,
    ) -> Result<send_state_event::v3::Response, BeaconError> {
        self.ensure_room_joined()?;

        let mut beacon_info_event = self.get_user_beacon_info(self.own_user_id()).await?;
        beacon_info_event.content.stop();
        Ok(self.send_state_event_for_key(self.own_user_id(), beacon_info_event.content).await?)
    }

    /// Send a location beacon event in the current room.
    ///
    /// # Arguments
    ///
    /// * `geo_uri` - The geo URI of the location beacon.
    ///
    /// # Errors
    ///
    /// Returns an error if the room is not joined, if the beacon information
    /// is redacted or stripped, if the location share is no longer live,
    /// or if the state event is not found.
    pub async fn send_location_beacon(
        &self,
        geo_uri: String,
    ) -> Result<send_message_event::v3::Response, BeaconError> {
        self.ensure_room_joined()?;

        let beacon_info_event = self.get_user_beacon_info(self.own_user_id()).await?;

        if beacon_info_event.content.is_live() {
            let content = BeaconEventContent::new(beacon_info_event.event_id, geo_uri, None);
            Ok(self.send(content).await?.response)
        } else {
            Err(BeaconError::NotLive)
        }
    }

    /// Store the given `ComposerDraft` in the state store using the current
    /// room id and optional thread root id as identifier.
    pub async fn save_composer_draft(
        &self,
        draft: ComposerDraft,
        thread_root: Option<&EventId>,
    ) -> Result<()> {
        self.client
            .state_store()
            .set_kv_data(
                StateStoreDataKey::ComposerDraft(self.room_id(), thread_root),
                StateStoreDataValue::ComposerDraft(draft),
            )
            .await?;
        Ok(())
    }

    /// Retrieve the `ComposerDraft` stored in the state store for this room
    /// and given thread, if any.
    pub async fn load_composer_draft(
        &self,
        thread_root: Option<&EventId>,
    ) -> Result<Option<ComposerDraft>> {
        let data = self
            .client
            .state_store()
            .get_kv_data(StateStoreDataKey::ComposerDraft(self.room_id(), thread_root))
            .await?;
        Ok(data.and_then(|d| d.into_composer_draft()))
    }

    /// Remove the `ComposerDraft` stored in the state store for this room
    /// and given thread, if any.
    pub async fn clear_composer_draft(&self, thread_root: Option<&EventId>) -> Result<()> {
        self.client
            .state_store()
            .remove_kv_data(StateStoreDataKey::ComposerDraft(self.room_id(), thread_root))
            .await?;
        Ok(())
    }

    /// Load pinned state events for a room from the `/state` endpoint in the
    /// home server.
    pub async fn load_pinned_events(&self) -> Result<Option<Vec<OwnedEventId>>> {
        let response = self
            .client
            .send(get_state_event_for_key::v3::Request::new(
                self.room_id().to_owned(),
                StateEventType::RoomPinnedEvents,
                "".to_owned(),
            ))
            .await;

        match response {
            Ok(response) => Ok(Some(
                response
                    .into_content()
                    .deserialize_as_unchecked::<RoomPinnedEventsEventContent>()?
                    .pinned,
            )),
            Err(http_error) => match http_error.as_client_api_error() {
                Some(error) if error.status_code == StatusCode::NOT_FOUND => Ok(None),
                _ => Err(http_error.into()),
            },
        }
    }

    /// Observe live location sharing events for this room.
    ///
    /// The returned observable will receive the newest event for each sync
    /// response that contains an `m.beacon` event.
    ///
    /// Returns a stream of [`ObservableLiveLocation`] events from other users
    /// in the room, excluding the live location events of the room's own user.
    pub fn observe_live_location_shares(&self) -> ObservableLiveLocation {
        ObservableLiveLocation::new(&self.client, self.room_id())
    }

    /// Subscribe to knock requests in this `Room`.
    ///
    /// The current requests to join the room will be emitted immediately
    /// when subscribing.
    ///
    /// A new set of knock requests will be emitted whenever:
    /// - A new member event is received.
    /// - A knock request is marked as seen.
    /// - A sync is gappy (limited), so room membership information may be
    ///   outdated.
    ///
    /// Returns both a stream of knock requests and a handle for a task that
    /// will clean up the seen knock request ids when possible.
    pub async fn subscribe_to_knock_requests(
        &self,
    ) -> Result<(impl Stream<Item = Vec<KnockRequest>> + use<>, JoinHandle<()>)> {
        let this = Arc::new(self.clone());

        let room_member_events_observer =
            self.client.observe_room_events::<SyncRoomMemberEvent, (Client, Room)>(this.room_id());

        let current_seen_ids = self.get_seen_knock_request_ids().await?;
        let mut seen_request_ids_stream = self
            .seen_knock_request_ids_map
            .subscribe()
            .await
            .map(|values| values.unwrap_or_default());

        let mut room_info_stream = self.subscribe_info();

        // Spawn a task that will clean up the seen knock request ids when updated room
        // members are received
        let clear_seen_ids_handle = spawn({
            let this = self.clone();
            async move {
                let mut member_updates_stream = this.room_member_updates_sender.subscribe();
                while member_updates_stream.recv().await.is_ok() {
                    // If room members were updated, try to remove outdated seen knock request ids
                    if let Err(err) = this.remove_outdated_seen_knock_requests_ids().await {
                        warn!("Failed to remove seen knock requests: {err}")
                    }
                }
            }
        });

        let combined_stream = stream! {
            // Emit current requests to join
            match this.get_current_join_requests(&current_seen_ids).await {
                Ok(initial_requests) => yield initial_requests,
                Err(err) => warn!("Failed to get initial requests to join: {err}")
            }

            let mut requests_stream = room_member_events_observer.subscribe();
            let mut seen_ids = current_seen_ids.clone();

            loop {
                // This is equivalent to a combine stream operation, triggering a new emission
                // when any of the branches changes
                tokio::select! {
                    Some((event, _)) = requests_stream.next() => {
                        if let Some(event) = event.as_original() {
                            // If we can calculate the membership change, try to emit only when needed
                            let emit = if event.prev_content().is_some() {
                                matches!(event.membership_change(),
                                    MembershipChange::Banned |
                                    MembershipChange::Knocked |
                                    MembershipChange::KnockAccepted |
                                    MembershipChange::KnockDenied |
                                    MembershipChange::KnockRetracted
                                )
                            } else {
                                // If we can't calculate the membership change, assume we need to
                                // emit updated values
                                true
                            };

                            if emit {
                                match this.get_current_join_requests(&seen_ids).await {
                                    Ok(requests) => yield requests,
                                    Err(err) => {
                                        warn!("Failed to get updated knock requests on new member event: {err}")
                                    }
                                }
                            }
                        }
                    }

                    Some(new_seen_ids) = seen_request_ids_stream.next() => {
                        // Update the current seen ids
                        seen_ids = new_seen_ids;

                        // If seen requests have changed we need to recalculate
                        // all the knock requests
                        match this.get_current_join_requests(&seen_ids).await {
                            Ok(requests) => yield requests,
                            Err(err) => {
                                warn!("Failed to get updated knock requests on seen ids changed: {err}")
                            }
                        }
                    }

                    Some(room_info) = room_info_stream.next() => {
                        // We need to emit new items when we may have missing room members:
                        // this usually happens after a gappy (limited) sync
                        if !room_info.are_members_synced() {
                            match this.get_current_join_requests(&seen_ids).await {
                                Ok(requests) => yield requests,
                                Err(err) => {
                                    warn!("Failed to get updated knock requests on gappy (limited) sync: {err}")
                                }
                            }
                        }
                    }
                    // If the streams in all branches are closed, stop the loop
                    else => break,
                }
            }
        };

        Ok((combined_stream, clear_seen_ids_handle))
    }

    async fn get_current_join_requests(
        &self,
        seen_request_ids: &BTreeMap<OwnedEventId, OwnedUserId>,
    ) -> Result<Vec<KnockRequest>> {
        Ok(self
            .members(RoomMemberships::KNOCK)
            .await?
            .into_iter()
            .filter_map(|member| {
                let event_id = member.event().event_id()?;
                Some(KnockRequest::new(
                    self,
                    event_id,
                    member.event().timestamp(),
                    KnockRequestMemberInfo::from_member(&member),
                    seen_request_ids.contains_key(event_id),
                ))
            })
            .collect())
    }

    /// Access the room settings related to privacy and visibility.
    pub fn privacy_settings(&self) -> RoomPrivacySettings<'_> {
        RoomPrivacySettings::new(&self.inner, &self.client)
    }

    /// Retrieve a list of all the threads for the current room.
    ///
    /// Since this client-server API is paginated, the return type may include a
    /// token used to resuming back-pagination into the list of results, in
    /// [`ThreadRoots::prev_batch_token`]. This token can be fed back into
    /// [`ListThreadsOptions::from`] to continue the pagination
    /// from the previous position.
    pub async fn list_threads(&self, opts: ListThreadsOptions) -> Result<ThreadRoots> {
        let request = opts.into_request(self.room_id());

        let response = self.client.send(request).await?;

        let push_ctx = self.push_context().await?;
        let chunk = join_all(
            response.chunk.into_iter().map(|ev| self.try_decrypt_event(ev, push_ctx.as_ref())),
        )
        .await;

        Ok(ThreadRoots { chunk, prev_batch_token: response.next_batch })
    }

    /// Retrieve a list of relations for the given event, according to the given
    /// options.
    ///
    /// Since this client-server API is paginated, the return type may include a
    /// token used to resuming back-pagination into the list of results, in
    /// [`Relations::prev_batch_token`]. This token can be fed back into
    /// [`RelationsOptions::from`] to continue the pagination from the previous
    /// position.
    ///
    /// **Note**: if [`RelationsOptions::from`] is set for a subsequent request,
    /// then it must be used with the same
    /// [`RelationsOptions::include_relations`] value as the request that
    /// returns the `from` token, otherwise the server behavior is undefined.
    pub async fn relations(
        &self,
        event_id: OwnedEventId,
        opts: RelationsOptions,
    ) -> Result<Relations> {
        let relations = opts.send(self, event_id).await;

        // Save any new related events to the cache.
        if let Ok(Relations { chunk, .. }) = &relations
            && let Ok((cache, _handles)) = self.event_cache().await
        {
            cache.save_events(chunk.clone()).await;
        }

        relations
    }

    /// Search this room's [`RoomIndex`] for query and return at most
    /// max_number_of_results results.
    #[cfg(feature = "experimental-search")]
    pub async fn search(
        &self,
        query: &str,
        max_number_of_results: usize,
        pagination_offset: Option<usize>,
    ) -> Result<Vec<OwnedEventId>, IndexError> {
        let mut search_index_guard = self.client.search_index().lock().await;
        search_index_guard.search(query, max_number_of_results, pagination_offset, self.room_id())
    }

    /// Subscribe to a given thread in this room.
    ///
    /// This will subscribe the user to the thread, so that they will receive
    /// notifications for that thread specifically.
    ///
    /// # Arguments
    ///
    /// - `thread_root`: The ID of the thread root event to subscribe to.
    /// - `automatic`: Whether the subscription was made automatically by a
    ///   client, not by manual user choice. If set, must include the latest
    ///   event ID that's known in the thread and that is causing the automatic
    ///   subscription. If unset (i.e. we're now subscribing manually) and there
    ///   was a previous automatic subscription, the subscription will be
    ///   overridden to a manual one instead.
    ///
    /// # Returns
    ///
    /// - A 404 error if the event isn't known, or isn't a thread root.
    /// - An `Ok` result if the subscription was successful, or if the server
    ///   skipped an automatic subscription (as the user unsubscribed from the
    ///   thread after the event causing the automatic subscription).
    #[instrument(skip(self), fields(room_id = %self.room_id()))]
    pub async fn subscribe_thread(
        &self,
        thread_root: OwnedEventId,
        automatic: Option<OwnedEventId>,
    ) -> Result<()> {
        let is_automatic = automatic.is_some();

        match self
            .client
            .send(subscribe_thread::unstable::Request::new(
                self.room_id().to_owned(),
                thread_root.clone(),
                automatic,
            ))
            .await
        {
            Ok(_response) => {
                trace!("Server acknowledged the thread subscription; saving in db");

                // Immediately save the result into the database.
                self.client
                    .state_store()
                    .upsert_thread_subscriptions(vec![(
                        self.room_id(),
                        &thread_root,
                        StoredThreadSubscription {
                            status: ThreadSubscriptionStatus::Subscribed {
                                automatic: is_automatic,
                            },
                            bump_stamp: None,
                        },
                    )])
                    .await?;

                Ok(())
            }

            Err(err) => {
                if let Some(ErrorKind::ConflictingUnsubscription) = err.client_api_error_kind() {
                    // In this case: the server indicates that the user unsubscribed *after* the
                    // event ID we've used in an automatic subscription; don't
                    // save the subscription state in the database, as the
                    // previous one should be more correct.
                    trace!("Thread subscription skipped: {err}");
                    Ok(())
                } else {
                    // Forward the error to the caller.
                    Err(err.into())
                }
            }
        }
    }

    /// Subscribe to a thread if needed, based on a current subscription to it.
    ///
    /// This is like [`Self::subscribe_thread`], but it first checks if the user
    /// has already subscribed to a thread, so as to minimize sending
    /// unnecessary subscriptions which would be ignored by the server.
    pub async fn subscribe_thread_if_needed(
        &self,
        thread_root: &EventId,
        automatic: Option<OwnedEventId>,
    ) -> Result<()> {
        if let Some(prev_sub) = self.load_or_fetch_thread_subscription(thread_root).await? {
            // If we have a previous subscription, we should only send the new one if it's
            // manual and the previous one was automatic.
            if !prev_sub.automatic || automatic.is_some() {
                // Either we had already a manual subscription, or we had an automatic one and
                // the new one is automatic too: nothing to do!
                return Ok(());
            }
        }
        self.subscribe_thread(thread_root.to_owned(), automatic).await
    }

    /// Unsubscribe from a given thread in this room.
    ///
    /// # Arguments
    ///
    /// - `thread_root`: The ID of the thread root event to unsubscribe to.
    ///
    /// # Returns
    ///
    /// - An `Ok` result if the unsubscription was successful, or the thread was
    ///   already unsubscribed.
    /// - A 404 error if the event isn't known, or isn't a thread root.
    #[instrument(skip(self), fields(room_id = %self.room_id()))]
    pub async fn unsubscribe_thread(&self, thread_root: OwnedEventId) -> Result<()> {
        self.client
            .send(unsubscribe_thread::unstable::Request::new(
                self.room_id().to_owned(),
                thread_root.clone(),
            ))
            .await?;

        trace!("Server acknowledged the thread subscription removal; removed it from db too");

        // Immediately save the result into the database.
        self.client
            .state_store()
            .upsert_thread_subscriptions(vec![(
                self.room_id(),
                &thread_root,
                StoredThreadSubscription {
                    status: ThreadSubscriptionStatus::Unsubscribed,
                    bump_stamp: None,
                },
            )])
            .await?;

        Ok(())
    }

    /// Return the current thread subscription for the given thread root in this
    /// room.
    ///
    /// # Arguments
    ///
    /// - `thread_root`: The ID of the thread root event to get the subscription
    ///   for.
    ///
    /// # Returns
    ///
    /// - An `Ok` result with `Some(ThreadSubscription)` if we have some
    ///   subscription information.
    /// - An `Ok` result with `None` if the subscription does not exist, or the
    ///   event couldn't be found, or the event isn't a thread.
    /// - An error if the request fails for any other reason, such as a network
    ///   error.
    #[instrument(skip(self), fields(room_id = %self.room_id()))]
    pub async fn fetch_thread_subscription(
        &self,
        thread_root: OwnedEventId,
    ) -> Result<Option<ThreadSubscription>> {
        let result = self
            .client
            .send(get_thread_subscription::unstable::Request::new(
                self.room_id().to_owned(),
                thread_root.clone(),
            ))
            .await;

        let subscription = match result {
            Ok(response) => Some(ThreadSubscription { automatic: response.automatic }),
            Err(http_error) => match http_error.as_client_api_error() {
                Some(error) if error.status_code == StatusCode::NOT_FOUND => None,
                _ => return Err(http_error.into()),
            },
        };

        // Keep the database in sync.
        if let Some(sub) = &subscription {
            self.client
                .state_store()
                .upsert_thread_subscriptions(vec![(
                    self.room_id(),
                    &thread_root,
                    StoredThreadSubscription {
                        status: ThreadSubscriptionStatus::Subscribed { automatic: sub.automatic },
                        bump_stamp: None,
                    },
                )])
                .await?;
        } else {
            // If the subscription was not found, remove it from the database.
            self.client
                .state_store()
                .remove_thread_subscription(self.room_id(), &thread_root)
                .await?;
        }

        Ok(subscription)
    }

    /// Return the current thread subscription for the given thread root in this
    /// room, by getting it from storage if possible, or fetching it from
    /// network otherwise.
    ///
    /// See also [`Self::fetch_thread_subscription`] for the exact semantics of
    /// this method.
    pub async fn load_or_fetch_thread_subscription(
        &self,
        thread_root: &EventId,
    ) -> Result<Option<ThreadSubscription>> {
        // If the thread subscriptions list is outdated, fetch from the server.
        if self.client.thread_subscription_catchup().is_outdated() {
            return self.fetch_thread_subscription(thread_root.to_owned()).await;
        }

        // Otherwise, we can rely on the store information.
        Ok(self
            .client
            .state_store()
            .load_thread_subscription(self.room_id(), thread_root)
            .await
            .map(|maybe_sub| {
                maybe_sub.and_then(|stored| match stored.status {
                    ThreadSubscriptionStatus::Unsubscribed => None,
                    ThreadSubscriptionStatus::Subscribed { automatic } => {
                        Some(ThreadSubscription { automatic })
                    }
                })
            })?)
    }
}

#[cfg(feature = "e2e-encryption")]
impl RoomIdentityProvider for Room {
    fn is_member<'a>(&'a self, user_id: &'a UserId) -> BoxFuture<'a, bool> {
        Box::pin(async { self.get_member(user_id).await.unwrap_or(None).is_some() })
    }

    fn member_identities(&self) -> BoxFuture<'_, Vec<UserIdentity>> {
        Box::pin(async {
            let members = self
                .members(RoomMemberships::JOIN | RoomMemberships::INVITE)
                .await
                .unwrap_or_else(|_| Default::default());

            let mut ret: Vec<UserIdentity> = Vec::new();
            for member in members {
                if let Some(i) = self.user_identity(member.user_id()).await {
                    ret.push(i);
                }
            }
            ret
        })
    }

    fn user_identity<'a>(&'a self, user_id: &'a UserId) -> BoxFuture<'a, Option<UserIdentity>> {
        Box::pin(async {
            self.client
                .encryption()
                .get_user_identity(user_id)
                .await
                .unwrap_or(None)
                .map(|u| u.underlying_identity())
        })
    }
}

/// A wrapper for a weak client and a room id that allows to lazily retrieve a
/// room, only when needed.
#[derive(Clone, Debug)]
pub(crate) struct WeakRoom {
    client: WeakClient,
    room_id: OwnedRoomId,
}

impl WeakRoom {
    /// Create a new `WeakRoom` given its weak components.
    pub fn new(client: WeakClient, room_id: OwnedRoomId) -> Self {
        Self { client, room_id }
    }

    /// Attempts to reconstruct the room.
    pub fn get(&self) -> Option<Room> {
        self.client.get().and_then(|client| client.get_room(&self.room_id))
    }

    /// The room id for that room.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }
}

/// Details of the (latest) invite.
#[derive(Debug, Clone)]
pub struct Invite {
    /// Who has been invited.
    pub invitee: RoomMember,
    /// Who sent the invite.
    pub inviter: Option<RoomMember>,
}

#[derive(Error, Debug)]
enum InvitationError {
    #[error("No membership event found")]
    EventMissing,
}

/// Receipts to send all at once.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Receipts {
    /// Fully-read marker (room account data).
    pub fully_read: Option<OwnedEventId>,
    /// Read receipt (public ephemeral room event).
    pub public_read_receipt: Option<OwnedEventId>,
    /// Read receipt (private ephemeral room event).
    pub private_read_receipt: Option<OwnedEventId>,
}

impl Receipts {
    /// Create an empty `Receipts`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the last event the user has read.
    ///
    /// It means that the user has read all the events before this event.
    ///
    /// This is a private marker only visible by the user.
    ///
    /// Note that this is technically not a receipt as it is persisted in the
    /// room account data.
    pub fn fully_read_marker(mut self, event_id: impl Into<Option<OwnedEventId>>) -> Self {
        self.fully_read = event_id.into();
        self
    }

    /// Set the last event presented to the user and forward it to the other
    /// users in the room.
    ///
    /// This is used to reset the unread messages/notification count and
    /// advertise to other users the last event that the user has likely seen.
    pub fn public_read_receipt(mut self, event_id: impl Into<Option<OwnedEventId>>) -> Self {
        self.public_read_receipt = event_id.into();
        self
    }

    /// Set the last event presented to the user and don't forward it.
    ///
    /// This is used to reset the unread messages/notification count.
    pub fn private_read_receipt(mut self, event_id: impl Into<Option<OwnedEventId>>) -> Self {
        self.private_read_receipt = event_id.into();
        self
    }

    /// Whether this `Receipts` is empty.
    pub fn is_empty(&self) -> bool {
        self.fully_read.is_none()
            && self.public_read_receipt.is_none()
            && self.private_read_receipt.is_none()
    }
}

/// [Parent space](https://spec.matrix.org/v1.8/client-server-api/#mspaceparent-relationships)
/// listed by a room, possibly validated by checking the space's state.
#[derive(Debug)]
pub enum ParentSpace {
    /// The room recognizes the given room as its parent, and the parent
    /// recognizes it as its child.
    Reciprocal(Room),
    /// The room recognizes the given room as its parent, but the parent does
    /// not recognizes it as its child. However, the author of the
    /// `m.space.parent` event in the room has a sufficient power level in the
    /// parent to create the child event.
    WithPowerlevel(Room),
    /// The room recognizes the given room as its parent, but the parent does
    /// not recognizes it as its child.
    Illegitimate(Room),
    /// The room recognizes the given id as its parent room, but we cannot check
    /// whether the parent recognizes it as its child.
    Unverifiable(OwnedRoomId),
}

/// The score to rate an inappropriate content.
///
/// Must be a value between `0`, inoffensive, and `-100`, very offensive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReportedContentScore(i8);

impl ReportedContentScore {
    /// The smallest value that can be represented by this type.
    ///
    /// This is for very offensive content.
    pub const MIN: Self = Self(-100);

    /// The largest value that can be represented by this type.
    ///
    /// This is for inoffensive content.
    pub const MAX: Self = Self(0);

    /// Try to create a `ReportedContentScore` from the provided `i8`.
    ///
    /// Returns `None` if it is smaller than [`ReportedContentScore::MIN`] or
    /// larger than [`ReportedContentScore::MAX`] .
    ///
    /// This is the same as the `TryFrom<i8>` implementation for
    /// `ReportedContentScore`, except that it returns an `Option` instead
    /// of a `Result`.
    pub fn new(value: i8) -> Option<Self> {
        value.try_into().ok()
    }

    /// Create a `ReportedContentScore` from the provided `i8` clamped to the
    /// acceptable interval.
    ///
    /// The given value gets clamped into the closed interval between
    /// [`ReportedContentScore::MIN`] and [`ReportedContentScore::MAX`].
    pub fn new_saturating(value: i8) -> Self {
        if value > Self::MAX {
            Self::MAX
        } else if value < Self::MIN {
            Self::MIN
        } else {
            Self(value)
        }
    }

    /// The value of this score.
    pub fn value(&self) -> i8 {
        self.0
    }
}

impl PartialEq<i8> for ReportedContentScore {
    fn eq(&self, other: &i8) -> bool {
        self.0.eq(other)
    }
}

impl PartialEq<ReportedContentScore> for i8 {
    fn eq(&self, other: &ReportedContentScore) -> bool {
        self.eq(&other.0)
    }
}

impl PartialOrd<i8> for ReportedContentScore {
    fn partial_cmp(&self, other: &i8) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(other)
    }
}

impl PartialOrd<ReportedContentScore> for i8 {
    fn partial_cmp(&self, other: &ReportedContentScore) -> Option<std::cmp::Ordering> {
        self.partial_cmp(&other.0)
    }
}

impl From<ReportedContentScore> for Int {
    fn from(value: ReportedContentScore) -> Self {
        value.0.into()
    }
}

impl TryFrom<i8> for ReportedContentScore {
    type Error = TryFromReportedContentScoreError;

    fn try_from(value: i8) -> std::prelude::v1::Result<Self, Self::Error> {
        if value > Self::MAX || value < Self::MIN {
            Err(TryFromReportedContentScoreError(()))
        } else {
            Ok(Self(value))
        }
    }
}

impl TryFrom<i16> for ReportedContentScore {
    type Error = TryFromReportedContentScoreError;

    fn try_from(value: i16) -> std::prelude::v1::Result<Self, Self::Error> {
        let value = i8::try_from(value).map_err(|_| TryFromReportedContentScoreError(()))?;
        value.try_into()
    }
}

impl TryFrom<i32> for ReportedContentScore {
    type Error = TryFromReportedContentScoreError;

    fn try_from(value: i32) -> std::prelude::v1::Result<Self, Self::Error> {
        let value = i8::try_from(value).map_err(|_| TryFromReportedContentScoreError(()))?;
        value.try_into()
    }
}

impl TryFrom<i64> for ReportedContentScore {
    type Error = TryFromReportedContentScoreError;

    fn try_from(value: i64) -> std::prelude::v1::Result<Self, Self::Error> {
        let value = i8::try_from(value).map_err(|_| TryFromReportedContentScoreError(()))?;
        value.try_into()
    }
}

impl TryFrom<Int> for ReportedContentScore {
    type Error = TryFromReportedContentScoreError;

    fn try_from(value: Int) -> std::prelude::v1::Result<Self, Self::Error> {
        let value = i8::try_from(value).map_err(|_| TryFromReportedContentScoreError(()))?;
        value.try_into()
    }
}

trait EventSource {
    fn get_event(
        &self,
        event_id: &EventId,
    ) -> impl Future<Output = Result<TimelineEvent, Error>> + SendOutsideWasm;
}

impl EventSource for &Room {
    async fn get_event(&self, event_id: &EventId) -> Result<TimelineEvent, Error> {
        self.load_or_fetch_event(event_id, None).await
    }
}

/// The error type returned when a checked `ReportedContentScore` conversion
/// fails.
#[derive(Debug, Clone, Error)]
#[error("out of range conversion attempted")]
pub struct TryFromReportedContentScoreError(());

/// Contains the current user's room member info and the optional room member
/// info of the sender of the `m.room.member` event that this info represents.
#[derive(Debug)]
pub struct RoomMemberWithSenderInfo {
    /// The actual room member.
    pub room_member: RoomMember,
    /// The info of the sender of the event `room_member` is based on, if
    /// available.
    pub sender_info: Option<RoomMember>,
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::collections::BTreeMap;

    use matrix_sdk_base::{ComposerDraft, DraftAttachment, store::ComposerDraftType};
    use matrix_sdk_test::{
        JoinedRoomBuilder, StateTestEvent, SyncResponseBuilder, async_test,
        event_factory::EventFactory, test_json,
    };
    use ruma::{
        RoomVersionId, event_id,
        events::{relation::RelationType, room::member::MembershipState},
        int, owned_event_id, room_id, user_id,
    };
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path_regex},
    };

    use super::ReportedContentScore;
    use crate::{
        Client,
        config::RequestConfig,
        room::messages::{IncludeRelations, ListThreadsOptions, RelationsOptions},
        test_utils::{
            client::mock_matrix_session,
            logged_in_client,
            mocks::{MatrixMockServer, RoomRelationsResponseTemplate},
        },
    };

    #[cfg(all(feature = "sqlite", feature = "e2e-encryption"))]
    #[async_test]
    async fn test_cache_invalidation_while_encrypt() {
        use matrix_sdk_base::store::RoomLoadSettings;
        use matrix_sdk_test::{DEFAULT_TEST_ROOM_ID, message_like_event_content};

        let sqlite_path = std::env::temp_dir().join("cache_invalidation_while_encrypt.db");
        let session = mock_matrix_session();

        let client = Client::builder()
            .homeserver_url("http://localhost:1234")
            .request_config(RequestConfig::new().disable_retry())
            .sqlite_store(&sqlite_path, None)
            .build()
            .await
            .unwrap();
        client
            .matrix_auth()
            .restore_session(session.clone(), RoomLoadSettings::default())
            .await
            .unwrap();

        client.encryption().enable_cross_process_store_lock("client1".to_owned()).await.unwrap();

        // Mock receiving an event to create an internal room.
        let server = MockServer::start().await;
        {
            Mock::given(method("GET"))
                .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.*room.*encryption.?"))
                .and(header("authorization", "Bearer 1234"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(&*test_json::sync_events::ENCRYPTION_CONTENT),
                )
                .mount(&server)
                .await;
            let response = SyncResponseBuilder::default()
                .add_joined_room(
                    JoinedRoomBuilder::default()
                        .add_state_event(StateTestEvent::Member)
                        .add_state_event(StateTestEvent::PowerLevels)
                        .add_state_event(StateTestEvent::Encryption),
                )
                .build_sync_response();
            client.base_client().receive_sync_response(response).await.unwrap();
        }

        let room = client.get_room(&DEFAULT_TEST_ROOM_ID).expect("Room should exist");

        // Step 1, preshare the room keys.
        room.preshare_room_key().await.unwrap();

        // Step 2, force lock invalidation by pretending another client obtained the
        // lock.
        {
            let client = Client::builder()
                .homeserver_url("http://localhost:1234")
                .request_config(RequestConfig::new().disable_retry())
                .sqlite_store(&sqlite_path, None)
                .build()
                .await
                .unwrap();
            client
                .matrix_auth()
                .restore_session(session.clone(), RoomLoadSettings::default())
                .await
                .unwrap();
            client
                .encryption()
                .enable_cross_process_store_lock("client2".to_owned())
                .await
                .unwrap();

            let guard = client.encryption().spin_lock_store(None).await.unwrap();
            assert!(guard.is_some());
        }

        // Step 3, take the crypto-store lock.
        let guard = client.encryption().spin_lock_store(None).await.unwrap();
        assert!(guard.is_some());

        // Step 4, try to encrypt a message.
        let olm = client.olm_machine().await;
        let olm = olm.as_ref().expect("Olm machine wasn't started");

        // Now pretend we're encrypting an event; the olm machine shouldn't rely on
        // caching the outgoing session before.
        let _encrypted_content = olm
            .encrypt_room_event_raw(room.room_id(), "test-event", &message_like_event_content!({}))
            .await
            .unwrap();
    }

    #[test]
    fn reported_content_score() {
        // i8
        let score = ReportedContentScore::new(0).unwrap();
        assert_eq!(score.value(), 0);
        let score = ReportedContentScore::new(-50).unwrap();
        assert_eq!(score.value(), -50);
        let score = ReportedContentScore::new(-100).unwrap();
        assert_eq!(score.value(), -100);
        assert_eq!(ReportedContentScore::new(10), None);
        assert_eq!(ReportedContentScore::new(-110), None);

        let score = ReportedContentScore::new_saturating(0);
        assert_eq!(score.value(), 0);
        let score = ReportedContentScore::new_saturating(-50);
        assert_eq!(score.value(), -50);
        let score = ReportedContentScore::new_saturating(-100);
        assert_eq!(score.value(), -100);
        let score = ReportedContentScore::new_saturating(10);
        assert_eq!(score, ReportedContentScore::MAX);
        let score = ReportedContentScore::new_saturating(-110);
        assert_eq!(score, ReportedContentScore::MIN);

        // i16
        let score = ReportedContentScore::try_from(0i16).unwrap();
        assert_eq!(score.value(), 0);
        let score = ReportedContentScore::try_from(-100i16).unwrap();
        assert_eq!(score.value(), -100);
        ReportedContentScore::try_from(10i16).unwrap_err();
        ReportedContentScore::try_from(-110i16).unwrap_err();

        // i32
        let score = ReportedContentScore::try_from(0i32).unwrap();
        assert_eq!(score.value(), 0);
        let score = ReportedContentScore::try_from(-100i32).unwrap();
        assert_eq!(score.value(), -100);
        ReportedContentScore::try_from(10i32).unwrap_err();
        ReportedContentScore::try_from(-110i32).unwrap_err();

        // i64
        let score = ReportedContentScore::try_from(0i64).unwrap();
        assert_eq!(score.value(), 0);
        let score = ReportedContentScore::try_from(-100i64).unwrap();
        assert_eq!(score.value(), -100);
        ReportedContentScore::try_from(10i64).unwrap_err();
        ReportedContentScore::try_from(-110i64).unwrap_err();

        // Int
        let score = ReportedContentScore::try_from(int!(0)).unwrap();
        assert_eq!(score.value(), 0);
        let score = ReportedContentScore::try_from(int!(-100)).unwrap();
        assert_eq!(score.value(), -100);
        ReportedContentScore::try_from(int!(10)).unwrap_err();
        ReportedContentScore::try_from(int!(-110)).unwrap_err();
    }

    #[async_test]
    async fn test_composer_draft() {
        use matrix_sdk_test::DEFAULT_TEST_ROOM_ID;

        let client = logged_in_client(None).await;

        let response = SyncResponseBuilder::default()
            .add_joined_room(JoinedRoomBuilder::default())
            .build_sync_response();
        client.base_client().receive_sync_response(response).await.unwrap();
        let room = client.get_room(&DEFAULT_TEST_ROOM_ID).expect("Room should exist");

        assert_eq!(room.load_composer_draft(None).await.unwrap(), None);

        // Save 2 drafts, one for the room and one for a thread.

        let draft = ComposerDraft {
            plain_text: "Hello, world!".to_owned(),
            html_text: Some("<strong>Hello</strong>, world!".to_owned()),
            draft_type: ComposerDraftType::NewMessage,
            attachments: vec![DraftAttachment {
                filename: "cat.txt".to_owned(),
                content: matrix_sdk_base::DraftAttachmentContent::File {
                    data: b"meow".to_vec(),
                    mimetype: Some("text/plain".to_owned()),
                    size: Some(5),
                },
            }],
        };

        room.save_composer_draft(draft.clone(), None).await.unwrap();

        let thread_root = owned_event_id!("$thread_root:b.c");
        let thread_draft = ComposerDraft {
            plain_text: "Hello, thread!".to_owned(),
            html_text: Some("<strong>Hello</strong>, thread!".to_owned()),
            draft_type: ComposerDraftType::NewMessage,
            attachments: vec![DraftAttachment {
                filename: "dog.txt".to_owned(),
                content: matrix_sdk_base::DraftAttachmentContent::File {
                    data: b"wuv".to_vec(),
                    mimetype: Some("text/plain".to_owned()),
                    size: Some(4),
                },
            }],
        };

        room.save_composer_draft(thread_draft.clone(), Some(&thread_root)).await.unwrap();

        // Check that the room draft was saved correctly
        assert_eq!(room.load_composer_draft(None).await.unwrap(), Some(draft));

        // Check that the thread draft was saved correctly
        assert_eq!(
            room.load_composer_draft(Some(&thread_root)).await.unwrap(),
            Some(thread_draft.clone())
        );

        // Clear the room draft
        room.clear_composer_draft(None).await.unwrap();
        assert_eq!(room.load_composer_draft(None).await.unwrap(), None);

        // Check that the thread one is still there
        assert_eq!(room.load_composer_draft(Some(&thread_root)).await.unwrap(), Some(thread_draft));

        // Clear the thread draft as well
        room.clear_composer_draft(Some(&thread_root)).await.unwrap();
        assert_eq!(room.load_composer_draft(Some(&thread_root)).await.unwrap(), None);
    }

    #[async_test]
    async fn test_mark_join_requests_as_seen() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let event_id = event_id!("$a:b.c");
        let room_id = room_id!("!a:b.c");
        let user_id = user_id!("@alice:b.c");

        let f = EventFactory::new().room(room_id);
        let joined_room_builder = JoinedRoomBuilder::new(room_id).add_state_bulk(vec![
            f.member(user_id).membership(MembershipState::Knock).event_id(event_id).into(),
        ]);
        let room = server.sync_room(&client, joined_room_builder).await;

        // When loading the initial seen ids, there are none
        let seen_ids =
            room.get_seen_knock_request_ids().await.expect("Couldn't load seen join request ids");
        assert!(seen_ids.is_empty());

        // We mark a random event id as seen
        room.mark_knock_requests_as_seen(&[user_id.to_owned()])
            .await
            .expect("Couldn't mark join request as seen");

        // Then we can check it was successfully marked as seen
        let seen_ids =
            room.get_seen_knock_request_ids().await.expect("Couldn't load seen join request ids");
        assert_eq!(seen_ids.len(), 1);
        assert_eq!(
            seen_ids.into_iter().next().expect("No next value"),
            (event_id.to_owned(), user_id.to_owned())
        )
    }

    #[async_test]
    async fn test_own_room_membership_with_no_own_member_event() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");

        let room = server.sync_joined_room(&client, room_id).await;

        // Since there is no member event for the own user, the method fails.
        // This should never happen in an actual room.
        let error = room.member_with_sender_info(client.user_id().unwrap()).await.err();
        assert!(error.is_some());
    }

    #[async_test]
    async fn test_own_room_membership_with_own_member_event_but_unknown_sender() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let user_id = user_id!("@example:localhost");

        let f = EventFactory::new().room(room_id).sender(user_id!("@alice:b.c"));
        let joined_room_builder =
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![f.member(user_id).into()]);
        let room = server.sync_room(&client, joined_room_builder).await;

        // When we load the membership details
        let ret = room
            .member_with_sender_info(client.user_id().unwrap())
            .await
            .expect("Room member info should be available");

        // We get the member info for the current user
        assert_eq!(ret.room_member.event().user_id(), user_id);

        // But there is no info for the sender
        assert!(ret.sender_info.is_none());
    }

    #[async_test]
    async fn test_own_room_membership_with_own_member_event_and_own_sender() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let user_id = user_id!("@example:localhost");

        let f = EventFactory::new().room(room_id).sender(user_id);
        let joined_room_builder =
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![f.member(user_id).into()]);
        let room = server.sync_room(&client, joined_room_builder).await;

        // When we load the membership details
        let ret = room
            .member_with_sender_info(client.user_id().unwrap())
            .await
            .expect("Room member info should be available");

        // We get the current user's member info
        assert_eq!(ret.room_member.event().user_id(), user_id);

        // And the sender has the same info, since it's also the current user
        assert!(ret.sender_info.is_some());
        assert_eq!(ret.sender_info.unwrap().event().user_id(), user_id);
    }

    #[async_test]
    async fn test_own_room_membership_with_own_member_event_and_known_sender() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let user_id = user_id!("@example:localhost");
        let sender_id = user_id!("@alice:b.c");

        let f = EventFactory::new().room(room_id).sender(sender_id);
        let joined_room_builder = JoinedRoomBuilder::new(room_id).add_state_bulk(vec![
            f.member(user_id).into(),
            // The sender info comes from the sync
            f.member(sender_id).into(),
        ]);
        let room = server.sync_room(&client, joined_room_builder).await;

        // When we load the membership details
        let ret = room
            .member_with_sender_info(client.user_id().unwrap())
            .await
            .expect("Room member info should be available");

        // We get the current user's member info
        assert_eq!(ret.room_member.event().user_id(), user_id);

        // And also the sender info from the events received in the sync
        assert!(ret.sender_info.is_some());
        assert_eq!(ret.sender_info.unwrap().event().user_id(), sender_id);
    }

    #[async_test]
    async fn test_own_room_membership_with_own_member_event_and_unknown_but_available_sender() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let user_id = user_id!("@example:localhost");
        let sender_id = user_id!("@alice:b.c");

        let f = EventFactory::new().room(room_id).sender(sender_id);
        let joined_room_builder =
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![f.member(user_id).into()]);
        let room = server.sync_room(&client, joined_room_builder).await;

        // We'll receive the member info through the /members endpoint
        server
            .mock_get_members()
            .ok(vec![f.member(sender_id).into_raw()])
            .mock_once()
            .mount()
            .await;

        // We get the current user's member info
        let ret = room
            .member_with_sender_info(client.user_id().unwrap())
            .await
            .expect("Room member info should be available");

        // We get the current user's member info
        assert_eq!(ret.room_member.event().user_id(), user_id);

        // And also the sender info from the /members endpoint
        assert!(ret.sender_info.is_some());
        assert_eq!(ret.sender_info.unwrap().event().user_id(), sender_id);
    }

    #[async_test]
    async fn test_list_threads() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");
        let f = EventFactory::new().room(room_id).sender(sender_id);

        let eid1 = event_id!("$1");
        let eid2 = event_id!("$2");
        let batch1 = vec![f.text_msg("Thread root 1").event_id(eid1).into_raw()];
        let batch2 = vec![f.text_msg("Thread root 2").event_id(eid2).into_raw()];

        server
            .mock_room_threads()
            .ok(batch1.clone(), Some("prev_batch".to_owned()))
            .mock_once()
            .mount()
            .await;
        server
            .mock_room_threads()
            .match_from("prev_batch")
            .ok(batch2, None)
            .mock_once()
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;
        let result =
            room.list_threads(ListThreadsOptions::default()).await.expect("Failed to list threads");
        assert_eq!(result.chunk.len(), 1);
        assert_eq!(result.chunk[0].event_id().unwrap(), eid1);
        assert!(result.prev_batch_token.is_some());

        let opts = ListThreadsOptions { from: result.prev_batch_token, ..Default::default() };
        let result = room.list_threads(opts).await.expect("Failed to list threads");
        assert_eq!(result.chunk.len(), 1);
        assert_eq!(result.chunk[0].event_id().unwrap(), eid2);
        assert!(result.prev_batch_token.is_none());
    }

    #[async_test]
    async fn test_relations() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");
        let f = EventFactory::new().room(room_id).sender(sender_id);

        let target_event_id = owned_event_id!("$target");
        let eid1 = event_id!("$1");
        let eid2 = event_id!("$2");
        let batch1 = vec![f.text_msg("Related event 1").event_id(eid1).into_raw()];
        let batch2 = vec![f.text_msg("Related event 2").event_id(eid2).into_raw()];

        server
            .mock_room_relations()
            .match_target_event(target_event_id.clone())
            .ok(RoomRelationsResponseTemplate::default().events(batch1).next_batch("next_batch"))
            .mock_once()
            .mount()
            .await;

        server
            .mock_room_relations()
            .match_target_event(target_event_id.clone())
            .match_from("next_batch")
            .ok(RoomRelationsResponseTemplate::default().events(batch2))
            .mock_once()
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;

        // Main endpoint: no relation type filtered out.
        let mut opts = RelationsOptions {
            include_relations: IncludeRelations::AllRelations,
            ..Default::default()
        };
        let result = room
            .relations(target_event_id.clone(), opts.clone())
            .await
            .expect("Failed to list relations the first time");
        assert_eq!(result.chunk.len(), 1);
        assert_eq!(result.chunk[0].event_id().unwrap(), eid1);
        assert!(result.prev_batch_token.is_none());
        assert!(result.next_batch_token.is_some());
        assert!(result.recursion_depth.is_none());

        opts.from = result.next_batch_token;
        let result = room
            .relations(target_event_id, opts)
            .await
            .expect("Failed to list relations the second time");
        assert_eq!(result.chunk.len(), 1);
        assert_eq!(result.chunk[0].event_id().unwrap(), eid2);
        assert!(result.prev_batch_token.is_none());
        assert!(result.next_batch_token.is_none());
        assert!(result.recursion_depth.is_none());
    }

    #[async_test]
    async fn test_relations_with_reltype() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");
        let f = EventFactory::new().room(room_id).sender(sender_id);

        let target_event_id = owned_event_id!("$target");
        let eid1 = event_id!("$1");
        let eid2 = event_id!("$2");
        let batch1 = vec![f.text_msg("In-thread event 1").event_id(eid1).into_raw()];
        let batch2 = vec![f.text_msg("In-thread event 2").event_id(eid2).into_raw()];

        server
            .mock_room_relations()
            .match_target_event(target_event_id.clone())
            .match_subrequest(IncludeRelations::RelationsOfType(RelationType::Thread))
            .ok(RoomRelationsResponseTemplate::default().events(batch1).next_batch("next_batch"))
            .mock_once()
            .mount()
            .await;

        server
            .mock_room_relations()
            .match_target_event(target_event_id.clone())
            .match_from("next_batch")
            .match_subrequest(IncludeRelations::RelationsOfType(RelationType::Thread))
            .ok(RoomRelationsResponseTemplate::default().events(batch2))
            .mock_once()
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;

        // Reltype-filtered endpoint, for threads \o/
        let mut opts = RelationsOptions {
            include_relations: IncludeRelations::RelationsOfType(RelationType::Thread),
            ..Default::default()
        };
        let result = room
            .relations(target_event_id.clone(), opts.clone())
            .await
            .expect("Failed to list relations the first time");
        assert_eq!(result.chunk.len(), 1);
        assert_eq!(result.chunk[0].event_id().unwrap(), eid1);
        assert!(result.prev_batch_token.is_none());
        assert!(result.next_batch_token.is_some());
        assert!(result.recursion_depth.is_none());

        opts.from = result.next_batch_token;
        let result = room
            .relations(target_event_id, opts)
            .await
            .expect("Failed to list relations the second time");
        assert_eq!(result.chunk.len(), 1);
        assert_eq!(result.chunk[0].event_id().unwrap(), eid2);
        assert!(result.prev_batch_token.is_none());
        assert!(result.next_batch_token.is_none());
        assert!(result.recursion_depth.is_none());
    }

    #[async_test]
    async fn test_power_levels_computation() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let sender_id = client.user_id().expect("No session id");
        let f = EventFactory::new().room(room_id).sender(sender_id);
        let mut user_map = BTreeMap::from([(sender_id.into(), 50.into())]);

        // Computing the power levels will need these 3 state events:
        let room_create_event = f.create(sender_id, RoomVersionId::V1).state_key("").into();
        let power_levels_event = f.power_levels(&mut user_map).state_key("").into();
        let room_member_event = f.member(sender_id).into();

        // With only the room member event
        let room = server
            .sync_room(&client, JoinedRoomBuilder::new(room_id).add_state_bulk([room_member_event]))
            .await;
        let ctx = room
            .push_condition_room_ctx()
            .await
            .expect("Failed to get push condition context")
            .expect("Could not get push condition context");

        // The internal power levels couldn't be computed
        assert!(ctx.power_levels.is_none());

        // Adding the room creation event
        let room = server
            .sync_room(&client, JoinedRoomBuilder::new(room_id).add_state_bulk([room_create_event]))
            .await;
        let ctx = room
            .push_condition_room_ctx()
            .await
            .expect("Failed to get push condition context")
            .expect("Could not get push condition context");

        // The internal power levels still couldn't be computed
        assert!(ctx.power_levels.is_none());

        // With the room member, room creation and the power levels events
        let room = server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id).add_state_bulk([power_levels_event]),
            )
            .await;
        let ctx = room
            .push_condition_room_ctx()
            .await
            .expect("Failed to get push condition context")
            .expect("Could not get push condition context");

        // The internal power levels can finally be computed
        assert!(ctx.power_levels.is_some());
    }
}
