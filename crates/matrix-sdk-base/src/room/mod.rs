// Copyright 2025 The Matrix.org Foundation C.I.C.
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

#![allow(clippy::assign_op_pattern)] // Triggered by bitflags! usage

mod call;
mod create;
mod display_name;
mod encryption;
mod knock;
mod latest_event;
mod members;
mod room_info;
mod state;
mod tags;
mod tombstone;

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

pub use create::*;
pub use display_name::{RoomDisplayName, RoomHero};
pub(crate) use display_name::{RoomSummary, UpdatedRoomDisplayName};
pub use encryption::EncryptionState;
use eyeball::{AsyncLock, SharedObservable};
use futures_util::{Stream, StreamExt};
pub use members::{RoomMember, RoomMembersUpdate, RoomMemberships};
pub(crate) use room_info::SyncInfo;
pub use room_info::{
    BaseRoomInfo, InviteAcceptanceDetails, RoomInfo, RoomInfoNotableUpdate,
    RoomInfoNotableUpdateReasons, RoomRecencyStamp, apply_redaction,
};
use ruma::{
    EventId, OwnedEventId, OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomId,
    RoomVersionId, UserId,
    events::{
        direct::OwnedDirectUserIdentifier,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::{
            avatar,
            guest_access::GuestAccess,
            history_visibility::HistoryVisibility,
            join_rules::JoinRule,
            power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent, RoomPowerLevelsSource},
        },
    },
    room::RoomType,
};
use serde::{Deserialize, Serialize};
pub use state::{RoomState, RoomStateFilter};
pub(crate) use tags::RoomNotableTags;
use tokio::sync::broadcast;
pub use tombstone::{PredecessorRoom, SuccessorRoom};
use tracing::{info, instrument, warn};

use crate::{
    Error, MinimalStateEvent,
    deserialized_responses::MemberEvent,
    notification_settings::RoomNotificationMode,
    read_receipts::RoomReadReceipts,
    store::{DynStateStore, Result as StoreResult, StateStoreExt},
    sync::UnreadNotificationsCount,
};

/// The underlying room data structure collecting state for joined, left and
/// invited rooms.
#[derive(Debug, Clone)]
pub struct Room {
    /// The room ID.
    pub(super) room_id: OwnedRoomId,

    /// Our own user ID.
    pub(super) own_user_id: OwnedUserId,

    pub(super) info: SharedObservable<RoomInfo>,
    pub(super) room_info_notable_update_sender: broadcast::Sender<RoomInfoNotableUpdate>,
    pub(super) store: Arc<DynStateStore>,

    /// A map for ids of room membership events in the knocking state linked to
    /// the user id of the user affected by the member event, that the current
    /// user has marked as seen so they can be ignored.
    pub seen_knock_request_ids_map:
        SharedObservable<Option<BTreeMap<OwnedEventId, OwnedUserId>>, AsyncLock>,

    /// A sender that will notify receivers when room member updates happen.
    pub room_member_updates_sender: broadcast::Sender<RoomMembersUpdate>,
}

impl Room {
    pub(crate) fn new(
        own_user_id: &UserId,
        store: Arc<DynStateStore>,
        room_id: &RoomId,
        room_state: RoomState,
        room_info_notable_update_sender: broadcast::Sender<RoomInfoNotableUpdate>,
    ) -> Self {
        let room_info = RoomInfo::new(room_id, room_state);
        Self::restore(own_user_id, store, room_info, room_info_notable_update_sender)
    }

    pub(crate) fn restore(
        own_user_id: &UserId,
        store: Arc<DynStateStore>,
        room_info: RoomInfo,
        room_info_notable_update_sender: broadcast::Sender<RoomInfoNotableUpdate>,
    ) -> Self {
        let (room_member_updates_sender, _) = broadcast::channel(10);
        Self {
            own_user_id: own_user_id.into(),
            room_id: room_info.room_id.clone(),
            store,
            info: SharedObservable::new(room_info),
            room_info_notable_update_sender,
            seen_knock_request_ids_map: SharedObservable::new_async(None),
            room_member_updates_sender,
        }
    }

    /// Get the unique room id of the room.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Get a copy of the room creators.
    pub fn creators(&self) -> Option<Vec<OwnedUserId>> {
        self.info.read().creators()
    }

    /// Get our own user id.
    pub fn own_user_id(&self) -> &UserId {
        &self.own_user_id
    }

    /// Whether this room's [`RoomType`] is `m.space`.
    pub fn is_space(&self) -> bool {
        self.info.read().room_type().is_some_and(|t| *t == RoomType::Space)
    }

    /// Returns the room's type as defined in its creation event
    /// (`m.room.create`).
    pub fn room_type(&self) -> Option<RoomType> {
        self.info.read().room_type().map(ToOwned::to_owned)
    }

    /// Get the unread notification counts.
    pub fn unread_notification_counts(&self) -> UnreadNotificationsCount {
        self.info.read().notification_counts
    }

    /// Get the number of unread messages (computed client-side).
    ///
    /// This might be more precise than [`Self::unread_notification_counts`] for
    /// encrypted rooms.
    pub fn num_unread_messages(&self) -> u64 {
        self.info.read().read_receipts.num_unread
    }

    /// Get the detailed information about read receipts for the room.
    pub fn read_receipts(&self) -> RoomReadReceipts {
        self.info.read().read_receipts.clone()
    }

    /// Get the number of unread notifications (computed client-side).
    ///
    /// This might be more precise than [`Self::unread_notification_counts`] for
    /// encrypted rooms.
    pub fn num_unread_notifications(&self) -> u64 {
        self.info.read().read_receipts.num_notifications
    }

    /// Get the number of unread mentions (computed client-side), that is,
    /// messages causing a highlight in a room.
    ///
    /// This might be more precise than [`Self::unread_notification_counts`] for
    /// encrypted rooms.
    pub fn num_unread_mentions(&self) -> u64 {
        self.info.read().read_receipts.num_mentions
    }

    /// Check if the room states have been synced
    ///
    /// States might be missing if we have only seen the room_id of this Room
    /// so far, for example as the response for a `create_room` request without
    /// being synced yet.
    ///
    /// Returns true if the state is fully synced, false otherwise.
    pub fn is_state_fully_synced(&self) -> bool {
        self.info.read().sync_info == SyncInfo::FullySynced
    }

    /// Check if the room state has been at least partially synced.
    ///
    /// See [`Room::is_state_fully_synced`] for more info.
    pub fn is_state_partially_or_fully_synced(&self) -> bool {
        self.info.read().sync_info != SyncInfo::NoState
    }

    /// Get the `prev_batch` token that was received from the last sync. May be
    /// `None` if the last sync contained the full room history.
    pub fn last_prev_batch(&self) -> Option<String> {
        self.info.read().last_prev_batch.clone()
    }

    /// Get the avatar url of this room.
    pub fn avatar_url(&self) -> Option<OwnedMxcUri> {
        self.info.read().avatar_url().map(ToOwned::to_owned)
    }

    /// Get information about the avatar of this room.
    pub fn avatar_info(&self) -> Option<avatar::ImageInfo> {
        self.info.read().avatar_info().map(ToOwned::to_owned)
    }

    /// Get the canonical alias of this room.
    pub fn canonical_alias(&self) -> Option<OwnedRoomAliasId> {
        self.info.read().canonical_alias().map(ToOwned::to_owned)
    }

    /// Get the canonical alias of this room.
    pub fn alt_aliases(&self) -> Vec<OwnedRoomAliasId> {
        self.info.read().alt_aliases().to_owned()
    }

    /// Get the `m.room.create` content of this room.
    ///
    /// This usually isn't optional but some servers might not send an
    /// `m.room.create` event as the first event for a given room, thus this can
    /// be optional.
    ///
    /// For room versions earlier than room version 11, if the event is
    /// redacted, all fields except `creator` will be set to their default
    /// value.
    pub fn create_content(&self) -> Option<RoomCreateWithCreatorEventContent> {
        match self.info.read().base_info.create.as_ref()? {
            MinimalStateEvent::Original(ev) => Some(ev.content.clone()),
            MinimalStateEvent::Redacted(ev) => Some(ev.content.clone()),
        }
    }

    /// Is this room considered a direct message.
    ///
    /// Async because it can read room info from storage.
    #[instrument(skip_all, fields(room_id = ?self.room_id))]
    pub async fn is_direct(&self) -> StoreResult<bool> {
        match self.state() {
            RoomState::Joined | RoomState::Left | RoomState::Banned => {
                Ok(!self.info.read().base_info.dm_targets.is_empty())
            }

            RoomState::Invited => {
                let member = self.get_member(self.own_user_id()).await?;

                match member {
                    None => {
                        info!("RoomMember not found for the user's own id");
                        Ok(false)
                    }
                    Some(member) => match member.event.as_ref() {
                        MemberEvent::Sync(_) => {
                            warn!("Got MemberEvent::Sync in an invited room");
                            Ok(false)
                        }
                        MemberEvent::Stripped(event) => {
                            Ok(event.content.is_direct.unwrap_or(false))
                        }
                    },
                }
            }

            // TODO: implement logic once we have the stripped events as we'd have with an Invite
            RoomState::Knocked => Ok(false),
        }
    }

    /// If this room is a direct message, get the members that we're sharing the
    /// room with.
    ///
    /// *Note*: The member list might have been modified in the meantime and
    /// the targets might not even be in the room anymore. This setting should
    /// only be considered as guidance. We leave members in this list to allow
    /// us to re-find a DM with a user even if they have left, since we may
    /// want to re-invite them.
    pub fn direct_targets(&self) -> HashSet<OwnedDirectUserIdentifier> {
        self.info.read().base_info.dm_targets.clone()
    }

    /// If this room is a direct message, returns the number of members that
    /// we're sharing the room with.
    pub fn direct_targets_length(&self) -> usize {
        self.info.read().base_info.dm_targets.len()
    }

    /// Get the guest access policy of this room.
    pub fn guest_access(&self) -> GuestAccess {
        self.info.read().guest_access().clone()
    }

    /// Get the history visibility policy of this room.
    pub fn history_visibility(&self) -> Option<HistoryVisibility> {
        self.info.read().history_visibility().cloned()
    }

    /// Get the history visibility policy of this room, or a sensible default if
    /// the event is missing.
    pub fn history_visibility_or_default(&self) -> HistoryVisibility {
        self.info.read().history_visibility_or_default().clone()
    }

    /// Is the room considered to be public.
    ///
    /// May return `None` if the join rule event is not available.
    pub fn is_public(&self) -> Option<bool> {
        self.info.read().join_rule().map(|join_rule| matches!(join_rule, JoinRule::Public))
    }

    /// Get the join rule policy of this room, if available.
    pub fn join_rule(&self) -> Option<JoinRule> {
        self.info.read().join_rule().cloned()
    }

    /// Get the maximum power level that this room contains.
    ///
    /// This is useful if one wishes to normalize the power levels, e.g. from
    /// 0-100 where 100 would be the max power level.
    pub fn max_power_level(&self) -> i64 {
        self.info.read().base_info.max_power_level
    }

    /// Get the current power levels of this room.
    pub async fn power_levels(&self) -> Result<RoomPowerLevels, Error> {
        let power_levels_content = self
            .store
            .get_state_event_static::<RoomPowerLevelsEventContent>(self.room_id())
            .await?
            .ok_or(Error::InsufficientData)?
            .deserialize()?;
        let creators = self.creators().ok_or(Error::InsufficientData)?;
        let rules = self.info.read().room_version_rules_or_default();

        Ok(power_levels_content.power_levels(&rules.authorization, creators))
    }

    /// Get the current power levels of this room, or a sensible default if they
    /// are not known.
    pub async fn power_levels_or_default(&self) -> RoomPowerLevels {
        if let Ok(power_levels) = self.power_levels().await {
            return power_levels;
        }

        // As a fallback, create the default power levels of a room.
        let rules = self.info.read().room_version_rules_or_default();
        RoomPowerLevels::new(
            RoomPowerLevelsSource::None,
            &rules.authorization,
            self.creators().into_iter().flatten(),
        )
    }

    /// Get the `m.room.name` of this room.
    ///
    /// The returned string may be empty if the event has been redacted, or it's
    /// missing from storage.
    pub fn name(&self) -> Option<String> {
        self.info.read().name().map(ToOwned::to_owned)
    }

    /// Get the topic of the room.
    pub fn topic(&self) -> Option<String> {
        self.info.read().topic().map(ToOwned::to_owned)
    }

    /// Update the cached user defined notification mode.
    ///
    /// This is automatically recomputed on every successful sync, and the
    /// cached result can be retrieved in
    /// [`Self::cached_user_defined_notification_mode`].
    pub fn update_cached_user_defined_notification_mode(&self, mode: RoomNotificationMode) {
        self.info.update_if(|info| {
            if info.cached_user_defined_notification_mode.as_ref() != Some(&mode) {
                info.cached_user_defined_notification_mode = Some(mode);

                true
            } else {
                false
            }
        });
    }

    /// Returns the cached user defined notification mode, if available.
    ///
    /// This cache is refilled every time we call
    /// [`Self::update_cached_user_defined_notification_mode`].
    pub fn cached_user_defined_notification_mode(&self) -> Option<RoomNotificationMode> {
        self.info.read().cached_user_defined_notification_mode
    }

    /// Removes any existing cached value for the user defined notification
    /// mode.
    pub fn clear_user_defined_notification_mode(&self) {
        self.info.update_if(|info| {
            if info.cached_user_defined_notification_mode.is_some() {
                info.cached_user_defined_notification_mode = None;
                true
            } else {
                false
            }
        })
    }

    /// Get the list of users ids that are considered to be joined members of
    /// this room.
    pub async fn joined_user_ids(&self) -> StoreResult<Vec<OwnedUserId>> {
        self.store.get_user_ids(self.room_id(), RoomMemberships::JOIN).await
    }

    /// Get the heroes for this room.
    pub fn heroes(&self) -> Vec<RoomHero> {
        self.info.read().heroes().to_vec()
    }

    /// Get the receipt as an `OwnedEventId` and `Receipt` tuple for the given
    /// `receipt_type`, `thread` and `user_id` in this room.
    pub async fn load_user_receipt(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> StoreResult<Option<(OwnedEventId, Receipt)>> {
        self.store.get_user_room_receipt_event(self.room_id(), receipt_type, thread, user_id).await
    }

    /// Load from storage the receipts as a list of `OwnedUserId` and `Receipt`
    /// tuples for the given `receipt_type`, `thread` and `event_id` in this
    /// room.
    pub async fn load_event_receipts(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> StoreResult<Vec<(OwnedUserId, Receipt)>> {
        self.store
            .get_event_room_receipt_events(self.room_id(), receipt_type, thread, event_id)
            .await
    }

    /// Returns a boolean indicating if this room has been manually marked as
    /// unread
    pub fn is_marked_unread(&self) -> bool {
        self.info.read().base_info.is_marked_unread
    }

    /// Returns the [`RoomVersionId`] of the room, if known.
    pub fn version(&self) -> Option<RoomVersionId> {
        self.info.read().room_version().cloned()
    }

    /// Returns the recency stamp of the room.
    ///
    /// Please read `RoomInfo::recency_stamp` to learn more.
    pub fn recency_stamp(&self) -> Option<RoomRecencyStamp> {
        self.info.read().recency_stamp
    }

    /// Returns the details about an invite to this room if the invite has been
    /// accepted by this specific client.
    ///
    /// # Returns
    /// - `Some` if an invite has been accepted by this specific client.
    /// - `None` if we didn't join this room using an invite or the invite
    ///   wasn't accepted by this client.
    pub fn invite_acceptance_details(&self) -> Option<InviteAcceptanceDetails> {
        self.info.read().invite_acceptance_details.clone()
    }

    /// Get a `Stream` of loaded pinned events for this room.
    /// If no pinned events are found a single empty `Vec` will be returned.
    pub fn pinned_event_ids_stream(&self) -> impl Stream<Item = Vec<OwnedEventId>> + use<> {
        self.info
            .subscribe()
            .map(|i| i.base_info.pinned_events.map(|c| c.pinned).unwrap_or_default())
    }

    /// Returns the current pinned event ids for this room.
    pub fn pinned_event_ids(&self) -> Option<Vec<OwnedEventId>> {
        self.info.read().pinned_event_ids()
    }
}

// See https://github.com/matrix-org/matrix-rust-sdk/pull/3749#issuecomment-2312939823.
#[cfg(not(feature = "test-send-sync"))]
unsafe impl Send for Room {}

// See https://github.com/matrix-org/matrix-rust-sdk/pull/3749#issuecomment-2312939823.
#[cfg(not(feature = "test-send-sync"))]
unsafe impl Sync for Room {}

#[cfg(feature = "test-send-sync")]
#[test]
// See https://github.com/matrix-org/matrix-rust-sdk/pull/3749#issuecomment-2312939823.
fn test_send_sync_for_room() {
    fn assert_send_sync<
        T: matrix_sdk_common::SendOutsideWasm + matrix_sdk_common::SyncOutsideWasm,
    >() {
    }

    assert_send_sync::<Room>();
}

/// The possible sources of an account data type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AccountDataSource {
    /// The source is account data with the stable prefix.
    Stable,

    /// The source is account data with the unstable prefix.
    #[default]
    Unstable,
}
