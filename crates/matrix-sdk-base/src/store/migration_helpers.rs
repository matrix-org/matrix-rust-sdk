// Copyright 2023 The Matrix.org Foundation C.I.C.
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

//! Data migration helpers for StateStore implementations.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

#[cfg(feature = "experimental-sliding-sync")]
use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
use ruma::{
    events::{
        room::{
            avatar::RoomAvatarEventContent,
            canonical_alias::RoomCanonicalAliasEventContent,
            create::RoomCreateEventContent,
            encryption::RoomEncryptionEventContent,
            guest_access::RoomGuestAccessEventContent,
            history_visibility::RoomHistoryVisibilityEventContent,
            join_rules::RoomJoinRulesEventContent,
            message::{ImageMessageEventContent, MessageType, RoomMessageEventContent},
            name::{RedactedRoomNameEventContent, RoomNameEventContent},
            tombstone::RoomTombstoneEventContent,
            topic::RoomTopicEventContent,
            MediaSource,
        },
        EmptyStateKey, EventContent, RedactContent, StateEventContent, StateEventType,
    },
    owned_mxc_uri, uint, OwnedRoomId, OwnedTransactionId, OwnedUserId, RoomId, TransactionId, UInt,
};
use serde::{Deserialize, Serialize};

use super::{
    ChildTransactionId, DependentQueuedRequest, DependentQueuedRequestKind, SentRequestKey,
    SerializableEventContent,
};
#[cfg(feature = "experimental-sliding-sync")]
use crate::latest_event::LatestEvent;
use crate::{
    deserialized_responses::SyncOrStrippedState,
    media::MediaRequestParameters,
    rooms::{
        normal::{RoomSummary, SyncInfo},
        BaseRoomInfo, RoomNotableTags,
    },
    sync::UnreadNotificationsCount,
    MinimalStateEvent, OriginalMinimalStateEvent, RoomInfo, RoomState,
};

/// [`RoomInfo`] version 1.
///
/// The `name` field in `RoomNameEventContent` was optional and has become
/// required. It means that sometimes the field has been serialized with the
/// value `null`.
///
/// For the migration:
///
/// 1. Deserialize the stored room info using this type,
/// 2. Get the `m.room.create` event for the room, if it is available,
/// 3. Convert this to [`RoomInfo`] with `.migrate(create_event)`,
/// 4. Replace the room info in the store.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomInfoV1 {
    room_id: OwnedRoomId,
    room_type: RoomState,
    notification_counts: UnreadNotificationsCount,
    summary: RoomSummary,
    members_synced: bool,
    last_prev_batch: Option<String>,
    #[serde(default = "sync_info_complete")] // see fn docs for why we use this default
    sync_info: SyncInfo,
    #[serde(default = "encryption_state_default")] // see fn docs for why we use this default
    encryption_state_synced: bool,
    #[cfg(feature = "experimental-sliding-sync")]
    latest_event: Option<SyncTimelineEvent>,
    base_info: BaseRoomInfoV1,
}

impl RoomInfoV1 {
    /// Get the room ID of this room.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Returns the state this room is in.
    pub fn state(&self) -> RoomState {
        self.room_type
    }

    /// Migrate this to a [`RoomInfo`], using the given `m.room.create` event
    /// from the room state.
    pub fn migrate(self, create: Option<&SyncOrStrippedState<RoomCreateEventContent>>) -> RoomInfo {
        let RoomInfoV1 {
            room_id,
            room_type,
            notification_counts,
            summary,
            members_synced,
            last_prev_batch,
            sync_info,
            encryption_state_synced,
            #[cfg(feature = "experimental-sliding-sync")]
            latest_event,
            base_info,
        } = self;

        RoomInfo {
            version: 0,
            room_id,
            room_state: room_type,
            prev_room_state: None,
            notification_counts,
            summary,
            members_synced,
            last_prev_batch,
            sync_info,
            encryption_state_synced,
            #[cfg(feature = "experimental-sliding-sync")]
            latest_event: latest_event.map(|ev| Box::new(LatestEvent::new(ev))),
            read_receipts: Default::default(),
            base_info: base_info.migrate(create),
            warned_about_unknown_room_version: Arc::new(false.into()),
            cached_display_name: None,
            cached_user_defined_notification_mode: None,
            #[cfg(feature = "experimental-sliding-sync")]
            recency_stamp: None,
        }
    }
}

// The sync_info field introduced a new field in the database schema, for
// backwards compatibility we assume that if the room is in the database, yet
// the field isn't, we have synced it before this field was introduced - which
// was a a full sync.
fn sync_info_complete() -> SyncInfo {
    SyncInfo::FullySynced
}

// The encryption_state_synced field introduced a new field in the database
// schema, for backwards compatibility we assume that if the room is in the
// database, yet the field isn't, we have synced it before this field was
// introduced - which was a a full sync.
fn encryption_state_default() -> bool {
    true
}

/// [`BaseRoomInfo`] version 1.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct BaseRoomInfoV1 {
    avatar: Option<MinimalStateEvent<RoomAvatarEventContent>>,
    canonical_alias: Option<MinimalStateEvent<RoomCanonicalAliasEventContent>>,
    dm_targets: HashSet<OwnedUserId>,
    encryption: Option<RoomEncryptionEventContent>,
    guest_access: Option<MinimalStateEvent<RoomGuestAccessEventContent>>,
    history_visibility: Option<MinimalStateEvent<RoomHistoryVisibilityEventContent>>,
    join_rules: Option<MinimalStateEvent<RoomJoinRulesEventContent>>,
    max_power_level: i64,
    name: Option<MinimalStateEvent<RoomNameEventContentV1>>,
    tombstone: Option<MinimalStateEvent<RoomTombstoneEventContent>>,
    topic: Option<MinimalStateEvent<RoomTopicEventContent>>,
}

impl BaseRoomInfoV1 {
    /// Migrate this to a [`BaseRoomInfo`].
    fn migrate(
        self,
        create: Option<&SyncOrStrippedState<RoomCreateEventContent>>,
    ) -> Box<BaseRoomInfo> {
        let BaseRoomInfoV1 {
            avatar,
            canonical_alias,
            dm_targets,
            encryption,
            guest_access,
            history_visibility,
            join_rules,
            max_power_level,
            name,
            tombstone,
            topic,
        } = self;

        let create = create.map(|ev| match ev {
            SyncOrStrippedState::Sync(e) => e.into(),
            SyncOrStrippedState::Stripped(e) => e.into(),
        });
        let name = name.map(|name| match name {
            MinimalStateEvent::Original(ev) => {
                MinimalStateEvent::Original(OriginalMinimalStateEvent {
                    content: ev.content.into(),
                    event_id: ev.event_id,
                })
            }
            MinimalStateEvent::Redacted(ev) => MinimalStateEvent::Redacted(ev),
        });

        Box::new(BaseRoomInfo {
            avatar,
            beacons: BTreeMap::new(),
            canonical_alias,
            create,
            dm_targets,
            encryption,
            guest_access,
            history_visibility,
            join_rules,
            max_power_level,
            name,
            tombstone,
            topic,
            rtc_member_events: BTreeMap::new(),
            is_marked_unread: false,
            notable_tags: RoomNotableTags::empty(),
            pinned_events: None,
        })
    }
}

/// [`RoomNameEventContent`] version 1, with an optional `name`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct RoomNameEventContentV1 {
    name: Option<String>,
}

impl EventContent for RoomNameEventContentV1 {
    type EventType = StateEventType;

    fn event_type(&self) -> Self::EventType {
        StateEventType::RoomName
    }
}

impl StateEventContent for RoomNameEventContentV1 {
    type StateKey = EmptyStateKey;
}

impl RedactContent for RoomNameEventContentV1 {
    type Redacted = RedactedRoomNameEventContent;

    fn redact(self, _version: &ruma::RoomVersionId) -> Self::Redacted {
        RedactedRoomNameEventContent::new()
    }
}

impl From<RoomNameEventContentV1> for RoomNameEventContent {
    fn from(value: RoomNameEventContentV1) -> Self {
        RoomNameEventContent::new(value.name.unwrap_or_default())
    }
}

/// [`DependentQueuedRequest`] v1.
///
/// See the docs of [`DependentQueuedRequestKindV1`] for details on the
/// migration.
#[derive(Debug, Serialize, Deserialize)]
pub struct DependentQueuedRequestV1 {
    /// Unique identifier for this dependent queued request.
    ///
    /// Useful for deletion.
    pub own_transaction_id: ChildTransactionId,

    /// The kind of user intent.
    pub kind: DependentQueuedRequestKindV1,

    /// Transaction id for the parent's local echo / used in the server request.
    ///
    /// Note: this is the transaction id used for the depended-on request, i.e.
    /// the one that was originally sent and that's being modified with this
    /// dependent request.
    pub parent_transaction_id: OwnedTransactionId,

    /// If the parent request has been sent, the parent's request identifier
    /// returned by the server once the local echo has been sent out.
    pub parent_key: Option<SentRequestKey>,
}

impl DependentQueuedRequestV1 {
    /// Construct a `DependentQueuedRequestV1` with the given
    /// `DependentQueuedRequestKindV1`.
    pub fn example_with_kind(kind: DependentQueuedRequestKindV1) -> Self {
        Self {
            own_transaction_id: ChildTransactionId::new(),
            kind,
            parent_transaction_id: TransactionId::new(),
            parent_key: None,
        }
    }

    /// Construct a `DependentQueuedRequestV1` with a
    /// `DependentQueuedRequestKindV1::FinishUpload{ .. }` that
    /// should be migrated.
    ///
    /// Returns the request and the transaction ID of the thumbnail, aka the
    /// `thumbnail_upload` of the new kind type.
    pub fn finish_upload_example() -> (Self, OwnedTransactionId) {
        let (kind, thumbnail_upload) = DependentQueuedRequestKindV1::finish_upload_example();
        (Self::example_with_kind(kind), thumbnail_upload)
    }
}

impl From<DependentQueuedRequestV1> for DependentQueuedRequest {
    fn from(value: DependentQueuedRequestV1) -> Self {
        Self {
            own_transaction_id: value.own_transaction_id,
            kind: value.kind.into(),
            parent_transaction_id: value.parent_transaction_id,
            parent_key: value.parent_key,
        }
    }
}

/// [`DependentQueuedRequestKind`] v1.
///
/// It used to contain the width and height of the thumbnail for the
/// `FinishUpload` variant.
///
/// With this migration in the `StateStore`, the associated media needs to be
/// migrated too in the `EventCacheStore`, by changing the `MediaRequest` to use
/// the `MediaFormat::File` instead of `MediaFormat::Thumbnail(_)`.
#[derive(Debug, Serialize, Deserialize)]
pub enum DependentQueuedRequestKindV1 {
    /// The event should be edited.
    EditEvent {
        /// The new event for the content.
        new_content: SerializableEventContent,
    },

    /// The event should be redacted/aborted/removed.
    RedactEvent,

    /// The event should be reacted to, with the given key.
    ReactEvent {
        /// Key used for the reaction.
        key: String,
    },

    /// Upload a file that had a thumbnail.
    UploadFileWithThumbnail {
        /// Content type for the file itself (not the thumbnail).
        content_type: String,

        /// Media request necessary to retrieve the file itself (not the
        /// thumbnail).
        cache_key: MediaRequestParameters,

        /// To which media transaction id does this upload relate to?
        related_to: OwnedTransactionId,
    },

    /// Finish an upload by updating references to the media cache and sending
    /// the final media event with the remote MXC URIs.
    FinishUpload {
        /// Local echo for the event (containing the local MXC URIs).
        local_echo: RoomMessageEventContent,

        /// Transaction id for the file upload.
        file_upload: OwnedTransactionId,

        /// Information about the thumbnail, if present.
        thumbnail_info: Option<FinishUploadThumbnailInfo>,
    },
}

impl DependentQueuedRequestKindV1 {
    /// Construct a `DependentQueuedRequestKindV1::FinishUpload{ .. }` that
    /// should be migrated.
    ///
    /// Returns the kind and the transaction ID of the thumbnail, aka the
    /// `thumbnail_upload` of the new type.
    pub fn finish_upload_example() -> (Self, OwnedTransactionId) {
        let local_echo = MessageType::Image(ImageMessageEventContent::new(
            "image.png".to_owned(),
            MediaSource::Plain(owned_mxc_uri!("mxc://homeserver.local/media")),
        ))
        .into();
        let file_upload = TransactionId::new();
        let thumbnail_upload = TransactionId::new();
        let thumbnail_info = FinishUploadThumbnailInfo {
            txn: thumbnail_upload.clone(),
            width: uint!(800),
            height: uint!(600),
        };

        let kind =
            Self::FinishUpload { local_echo, file_upload, thumbnail_info: Some(thumbnail_info) };
        (kind, thumbnail_upload)
    }
}

impl From<DependentQueuedRequestKindV1> for DependentQueuedRequestKind {
    fn from(value: DependentQueuedRequestKindV1) -> Self {
        match value {
            DependentQueuedRequestKindV1::EditEvent { new_content } => {
                Self::EditEvent { new_content }
            }
            DependentQueuedRequestKindV1::RedactEvent => Self::RedactEvent,
            DependentQueuedRequestKindV1::ReactEvent { key } => Self::ReactEvent { key },
            DependentQueuedRequestKindV1::UploadFileWithThumbnail {
                content_type,
                cache_key,
                related_to,
            } => Self::UploadFileWithThumbnail { content_type, cache_key, related_to },
            DependentQueuedRequestKindV1::FinishUpload {
                local_echo,
                file_upload,
                thumbnail_info,
            } => Self::FinishUpload {
                local_echo,
                file_upload,
                thumbnail_upload: thumbnail_info.map(|info| info.txn),
            },
        }
    }
}

/// Detailed record about a thumbnail used when finishing a media upload.
#[derive(Debug, Serialize, Deserialize)]
pub struct FinishUploadThumbnailInfo {
    /// Transaction id for the thumbnail upload.
    pub txn: OwnedTransactionId,
    /// Thumbnail's width.
    pub width: UInt,
    /// Thumbnail's height.
    pub height: UInt,
}
