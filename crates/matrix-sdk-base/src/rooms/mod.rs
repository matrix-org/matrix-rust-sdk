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

mod display_name;
mod encryption;
mod latest_event;
mod members;
mod room_info;
mod state;
mod tags;

use crate::{
    deserialized_responses::{MemberEvent, RawMemberEvent, SyncOrStrippedState},
    notification_settings::RoomNotificationMode,
    read_receipts::RoomReadReceipts,
    store::{DynStateStore, Result as StoreResult, StateStoreExt},
    sync::UnreadNotificationsCount,
    Error, MinimalStateEvent, StateStoreDataKey, StateStoreDataValue, StoreError,
};
use as_variant::as_variant;
pub use display_name::{RoomDisplayName, RoomHero};
pub(crate) use display_name::{RoomSummary, UpdatedRoomDisplayName};
pub use encryption::EncryptionState;
use eyeball::{AsyncLock, ObservableWriteGuard, SharedObservable};
use futures_util::{Stream, StreamExt};
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_common::ring_buffer::RingBuffer;
pub use members::{RoomMember, RoomMembersUpdate, RoomMemberships};
pub(crate) use room_info::SyncInfo;
pub use room_info::{
    apply_redaction, BaseRoomInfo, RoomInfo, RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons,
};
use ruma::{
    assign,
    events::{
        direct::OwnedDirectUserIdentifier,
        macros::EventContent,
        member_hints::MemberHintsEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::create::{PreviousRoom, RoomCreateEventContent},
        room::{
            avatar::{self},
            guest_access::GuestAccess,
            history_visibility::HistoryVisibility,
            join_rules::JoinRule,
            member::{MembershipState, RoomMemberEventContent},
            power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
            tombstone::RoomTombstoneEventContent,
        },
        EmptyStateKey, RedactContent, RedactedStateEventContent, StateEventType, SyncStateEvent,
    },
    room::RoomType,
    EventId, OwnedEventId, OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomId,
    RoomVersionId, UserId,
};
#[cfg(feature = "e2e-encryption")]
use ruma::{events::AnySyncTimelineEvent, serde::Raw};
use serde::{Deserialize, Serialize};
pub use state::{RoomState, RoomStateFilter};
#[cfg(feature = "e2e-encryption")]
use std::sync::RwLock as SyncRwLock;
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};
pub(crate) use tags::RoomNotableTags;
use tokio::sync::broadcast;
use tracing::{info, instrument, warn};

/// The underlying room data structure collecting state for joined, left and
/// invited rooms.
#[derive(Debug, Clone)]
pub struct Room {
    /// The room ID.
    pub(super) room_id: OwnedRoomId,

    /// Our own user ID.
    pub(super) own_user_id: OwnedUserId,

    pub(super) inner: SharedObservable<RoomInfo>,
    pub(super) room_info_notable_update_sender: broadcast::Sender<RoomInfoNotableUpdate>,
    pub(super) store: Arc<DynStateStore>,

    /// The most recent few encrypted events. When the keys come through to
    /// decrypt these, the most recent relevant one will replace
    /// `latest_event`. (We can't tell which one is relevant until
    /// they are decrypted.)
    ///
    /// Currently, these are held in Room rather than RoomInfo, because we were
    /// not sure whether holding too many of them might make the cache too
    /// slow to load on startup. Keeping them here means they are not cached
    /// to disk but held in memory.
    #[cfg(feature = "e2e-encryption")]
    pub latest_encrypted_events: Arc<SyncRwLock<RingBuffer<Raw<AnySyncTimelineEvent>>>>,

    /// A map for ids of room membership events in the knocking state linked to
    /// the user id of the user affected by the member event, that the current
    /// user has marked as seen so they can be ignored.
    pub seen_knock_request_ids_map:
        SharedObservable<Option<BTreeMap<OwnedEventId, OwnedUserId>>, AsyncLock>,

    /// A sender that will notify receivers when room member updates happen.
    pub room_member_updates_sender: broadcast::Sender<RoomMembersUpdate>,
}

impl Room {
    /// The size of the latest_encrypted_events RingBuffer
    #[cfg(feature = "e2e-encryption")]
    const MAX_ENCRYPTED_EVENTS: std::num::NonZeroUsize = std::num::NonZeroUsize::new(10).unwrap();

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
            inner: SharedObservable::new(room_info),
            #[cfg(feature = "e2e-encryption")]
            latest_encrypted_events: Arc::new(SyncRwLock::new(RingBuffer::new(
                Self::MAX_ENCRYPTED_EVENTS,
            ))),
            room_info_notable_update_sender,
            seen_knock_request_ids_map: SharedObservable::new_async(None),
            room_member_updates_sender,
        }
    }

    /// Get the unique room id of the room.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Get a copy of the room creator.
    pub fn creator(&self) -> Option<OwnedUserId> {
        self.inner.read().creator().map(ToOwned::to_owned)
    }

    /// Get our own user id.
    pub fn own_user_id(&self) -> &UserId {
        &self.own_user_id
    }

    /// Whether this room's [`RoomType`] is `m.space`.
    pub fn is_space(&self) -> bool {
        self.inner.read().room_type().is_some_and(|t| *t == RoomType::Space)
    }

    /// Returns the room's type as defined in its creation event
    /// (`m.room.create`).
    pub fn room_type(&self) -> Option<RoomType> {
        self.inner.read().room_type().map(ToOwned::to_owned)
    }

    /// Get the unread notification counts.
    pub fn unread_notification_counts(&self) -> UnreadNotificationsCount {
        self.inner.read().notification_counts
    }

    /// Get the number of unread messages (computed client-side).
    ///
    /// This might be more precise than [`Self::unread_notification_counts`] for
    /// encrypted rooms.
    pub fn num_unread_messages(&self) -> u64 {
        self.inner.read().read_receipts.num_unread
    }

    /// Get the detailed information about read receipts for the room.
    pub fn read_receipts(&self) -> RoomReadReceipts {
        self.inner.read().read_receipts.clone()
    }

    /// Get the number of unread notifications (computed client-side).
    ///
    /// This might be more precise than [`Self::unread_notification_counts`] for
    /// encrypted rooms.
    pub fn num_unread_notifications(&self) -> u64 {
        self.inner.read().read_receipts.num_notifications
    }

    /// Get the number of unread mentions (computed client-side), that is,
    /// messages causing a highlight in a room.
    ///
    /// This might be more precise than [`Self::unread_notification_counts`] for
    /// encrypted rooms.
    pub fn num_unread_mentions(&self) -> u64 {
        self.inner.read().read_receipts.num_mentions
    }

    /// Check if the room states have been synced
    ///
    /// States might be missing if we have only seen the room_id of this Room
    /// so far, for example as the response for a `create_room` request without
    /// being synced yet.
    ///
    /// Returns true if the state is fully synced, false otherwise.
    pub fn is_state_fully_synced(&self) -> bool {
        self.inner.read().sync_info == SyncInfo::FullySynced
    }

    /// Check if the room state has been at least partially synced.
    ///
    /// See [`Room::is_state_fully_synced`] for more info.
    pub fn is_state_partially_or_fully_synced(&self) -> bool {
        self.inner.read().sync_info != SyncInfo::NoState
    }

    /// Get the `prev_batch` token that was received from the last sync. May be
    /// `None` if the last sync contained the full room history.
    pub fn last_prev_batch(&self) -> Option<String> {
        self.inner.read().last_prev_batch.clone()
    }

    /// Get the avatar url of this room.
    pub fn avatar_url(&self) -> Option<OwnedMxcUri> {
        self.inner.read().avatar_url().map(ToOwned::to_owned)
    }

    /// Get information about the avatar of this room.
    pub fn avatar_info(&self) -> Option<avatar::ImageInfo> {
        self.inner.read().avatar_info().map(ToOwned::to_owned)
    }

    /// Get the canonical alias of this room.
    pub fn canonical_alias(&self) -> Option<OwnedRoomAliasId> {
        self.inner.read().canonical_alias().map(ToOwned::to_owned)
    }

    /// Get the canonical alias of this room.
    pub fn alt_aliases(&self) -> Vec<OwnedRoomAliasId> {
        self.inner.read().alt_aliases().to_owned()
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
        match self.inner.read().base_info.create.as_ref()? {
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
                Ok(!self.inner.read().base_info.dm_targets.is_empty())
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
        self.inner.read().base_info.dm_targets.clone()
    }

    /// If this room is a direct message, returns the number of members that
    /// we're sharing the room with.
    pub fn direct_targets_length(&self) -> usize {
        self.inner.read().base_info.dm_targets.len()
    }

    /// Get the guest access policy of this room.
    pub fn guest_access(&self) -> GuestAccess {
        self.inner.read().guest_access().clone()
    }

    /// Get the history visibility policy of this room.
    pub fn history_visibility(&self) -> Option<HistoryVisibility> {
        self.inner.read().history_visibility().cloned()
    }

    /// Get the history visibility policy of this room, or a sensible default if
    /// the event is missing.
    pub fn history_visibility_or_default(&self) -> HistoryVisibility {
        self.inner.read().history_visibility_or_default().clone()
    }

    /// Is the room considered to be public.
    pub fn is_public(&self) -> bool {
        matches!(self.join_rule(), JoinRule::Public)
    }

    /// Get the join rule policy of this room.
    pub fn join_rule(&self) -> JoinRule {
        self.inner.read().join_rule().clone()
    }

    /// Get the maximum power level that this room contains.
    ///
    /// This is useful if one wishes to normalize the power levels, e.g. from
    /// 0-100 where 100 would be the max power level.
    pub fn max_power_level(&self) -> i64 {
        self.inner.read().base_info.max_power_level
    }

    /// Get the current power levels of this room.
    pub async fn power_levels(&self) -> Result<RoomPowerLevels, Error> {
        Ok(self
            .store
            .get_state_event_static::<RoomPowerLevelsEventContent>(self.room_id())
            .await?
            .ok_or(Error::InsufficientData)?
            .deserialize()?
            .power_levels())
    }

    /// Get the `m.room.name` of this room.
    ///
    /// The returned string may be empty if the event has been redacted, or it's
    /// missing from storage.
    pub fn name(&self) -> Option<String> {
        self.inner.read().name().map(ToOwned::to_owned)
    }

    /// Has the room been tombstoned.
    pub fn is_tombstoned(&self) -> bool {
        self.inner.read().base_info.tombstone.is_some()
    }

    /// Get the `m.room.tombstone` content of this room if there is one.
    pub fn tombstone(&self) -> Option<RoomTombstoneEventContent> {
        self.inner.read().tombstone().cloned()
    }

    /// Get the topic of the room.
    pub fn topic(&self) -> Option<String> {
        self.inner.read().topic().map(ToOwned::to_owned)
    }

    /// Is there a non expired membership with application "m.call" and scope
    /// "m.room" in this room
    pub fn has_active_room_call(&self) -> bool {
        self.inner.read().has_active_room_call()
    }

    /// Returns a Vec of userId's that participate in the room call.
    ///
    /// MatrixRTC memberships with application "m.call" and scope "m.room" are
    /// considered. A user can occur twice if they join with two devices.
    /// convert to a set depending if the different users are required or the
    /// amount of sessions.
    ///
    /// The vector is ordered by oldest membership user to newest.
    pub fn active_room_call_participants(&self) -> Vec<OwnedUserId> {
        self.inner.read().active_room_call_participants()
    }

    pub(super) async fn get_member_hints(&self) -> StoreResult<MemberHintsEventContent> {
        Ok(self
            .store
            .get_state_event_static::<MemberHintsEventContent>(self.room_id())
            .await?
            .and_then(|event| {
                event
                    .deserialize()
                    .inspect_err(|e| warn!("Couldn't deserialize the member hints event: {e}"))
                    .ok()
            })
            .and_then(|event| as_variant!(event, SyncOrStrippedState::Sync(SyncStateEvent::Original(e)) => e.content))
            .unwrap_or_default())
    }

    /// Update the cached user defined notification mode.
    ///
    /// This is automatically recomputed on every successful sync, and the
    /// cached result can be retrieved in
    /// [`Self::cached_user_defined_notification_mode`].
    pub fn update_cached_user_defined_notification_mode(&self, mode: RoomNotificationMode) {
        self.inner.update_if(|info| {
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
        self.inner.read().cached_user_defined_notification_mode
    }

    /// Get the list of users ids that are considered to be joined members of
    /// this room.
    pub async fn joined_user_ids(&self) -> StoreResult<Vec<OwnedUserId>> {
        self.store.get_user_ids(self.room_id(), RoomMemberships::JOIN).await
    }

    /// Get the heroes for this room.
    pub fn heroes(&self) -> Vec<RoomHero> {
        self.inner.read().heroes().to_vec()
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
        self.inner.read().base_info.is_marked_unread
    }

    /// Returns the recency stamp of the room.
    ///
    /// Please read `RoomInfo::recency_stamp` to learn more.
    pub fn recency_stamp(&self) -> Option<u64> {
        self.inner.read().recency_stamp
    }

    /// Get a `Stream` of loaded pinned events for this room.
    /// If no pinned events are found a single empty `Vec` will be returned.
    pub fn pinned_event_ids_stream(&self) -> impl Stream<Item = Vec<OwnedEventId>> {
        self.inner
            .subscribe()
            .map(|i| i.base_info.pinned_events.map(|c| c.pinned).unwrap_or_default())
    }

    /// Returns the current pinned event ids for this room.
    pub fn pinned_event_ids(&self) -> Option<Vec<OwnedEventId>> {
        self.inner.read().pinned_event_ids()
    }

    /// Mark a list of requests to join the room as seen, given their state
    /// event ids.
    pub async fn mark_knock_requests_as_seen(&self, user_ids: &[OwnedUserId]) -> StoreResult<()> {
        let raw_user_ids: Vec<&str> = user_ids.iter().map(|id| id.as_str()).collect();
        let member_raw_events = self
            .store
            .get_state_events_for_keys(self.room_id(), StateEventType::RoomMember, &raw_user_ids)
            .await?;
        let mut event_to_user_ids = Vec::with_capacity(member_raw_events.len());

        // Map the list of events ids to their user ids, if they are event ids for knock
        // membership events. Log an error and continue otherwise.
        for raw_event in member_raw_events {
            let event = raw_event.cast::<RoomMemberEventContent>().deserialize()?;
            match event {
                SyncOrStrippedState::Sync(SyncStateEvent::Original(event)) => {
                    if event.content.membership == MembershipState::Knock {
                        event_to_user_ids.push((event.event_id, event.state_key))
                    } else {
                        warn!("Could not mark knock event as seen: event {} for user {} is not in Knock membership state.", event.event_id, event.state_key);
                    }
                }
                _ => warn!(
                    "Could not mark knock event as seen: event for user {} is not valid.",
                    event.state_key()
                ),
            }
        }

        let current_seen_events_guard = self.get_write_guarded_current_knock_request_ids().await?;
        let mut current_seen_events = current_seen_events_guard.clone().unwrap_or_default();

        current_seen_events.extend(event_to_user_ids);

        self.update_seen_knock_request_ids(current_seen_events_guard, current_seen_events).await?;

        Ok(())
    }

    /// Removes the seen knock request ids that are no longer valid given the
    /// current room members.
    pub async fn remove_outdated_seen_knock_requests_ids(&self) -> StoreResult<()> {
        let current_seen_events_guard = self.get_write_guarded_current_knock_request_ids().await?;
        let mut current_seen_events = current_seen_events_guard.clone().unwrap_or_default();

        // Get and deserialize the member events for the seen knock requests
        let keys: Vec<OwnedUserId> = current_seen_events.values().map(|id| id.to_owned()).collect();
        let raw_member_events: Vec<RawMemberEvent> =
            self.store.get_state_events_for_keys_static(self.room_id(), &keys).await?;
        let member_events = raw_member_events
            .into_iter()
            .map(|raw| raw.deserialize())
            .collect::<Result<Vec<MemberEvent>, _>>()?;

        let mut ids_to_remove = Vec::new();

        for (event_id, user_id) in current_seen_events.iter() {
            // Check the seen knock request ids against the current room member events for
            // the room members associated to them
            let matching_member = member_events.iter().find(|event| event.user_id() == user_id);

            if let Some(member) = matching_member {
                let member_event_id = member.event_id();
                // If the member event is not a knock or it's different knock, it's outdated
                if *member.membership() != MembershipState::Knock
                    || member_event_id.is_some_and(|id| id != event_id)
                {
                    ids_to_remove.push(event_id.to_owned());
                }
            } else {
                ids_to_remove.push(event_id.to_owned());
            }
        }

        // If there are no ids to remove, do nothing
        if ids_to_remove.is_empty() {
            return Ok(());
        }

        for event_id in ids_to_remove {
            current_seen_events.remove(&event_id);
        }

        self.update_seen_knock_request_ids(current_seen_events_guard, current_seen_events).await?;

        Ok(())
    }

    /// Get the list of seen knock request event ids in this room.
    pub async fn get_seen_knock_request_ids(
        &self,
    ) -> Result<BTreeMap<OwnedEventId, OwnedUserId>, StoreError> {
        Ok(self.get_write_guarded_current_knock_request_ids().await?.clone().unwrap_or_default())
    }

    async fn get_write_guarded_current_knock_request_ids(
        &self,
    ) -> StoreResult<ObservableWriteGuard<'_, Option<BTreeMap<OwnedEventId, OwnedUserId>>, AsyncLock>>
    {
        let mut guard = self.seen_knock_request_ids_map.write().await;
        // If there are no loaded request ids yet
        if guard.is_none() {
            // Load the values from the store and update the shared observable contents
            let updated_seen_ids = self
                .store
                .get_kv_data(StateStoreDataKey::SeenKnockRequests(self.room_id()))
                .await?
                .and_then(|v| v.into_seen_knock_requests())
                .unwrap_or_default();

            ObservableWriteGuard::set(&mut guard, Some(updated_seen_ids));
        }
        Ok(guard)
    }

    async fn update_seen_knock_request_ids(
        &self,
        mut guard: ObservableWriteGuard<'_, Option<BTreeMap<OwnedEventId, OwnedUserId>>, AsyncLock>,
        new_value: BTreeMap<OwnedEventId, OwnedUserId>,
    ) -> StoreResult<()> {
        // Save the new values to the shared observable
        ObservableWriteGuard::set(&mut guard, Some(new_value.clone()));

        // Save them into the store too
        self.store
            .set_kv_data(
                StateStoreDataKey::SeenKnockRequests(self.room_id()),
                StateStoreDataValue::SeenKnockRequests(new_value),
            )
            .await?;

        Ok(())
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
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<Room>();
}

#[cfg(test)]
mod tests {
    use std::{
        ops::Sub,
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use assign::assign;
    use matrix_sdk_test::{ALICE, BOB, CAROL};
    use ruma::{
        device_id, event_id,
        events::{
            call::member::{
                ActiveFocus, ActiveLivekitFocus, Application, CallApplicationContent,
                CallMemberEventContent, CallMemberStateKey, Focus, LegacyMembershipData,
                LegacyMembershipDataInit, LivekitFocus, OriginalSyncCallMemberEvent,
            },
            AnySyncStateEvent, StateUnsigned, SyncStateEvent,
        },
        room_id, user_id, DeviceId, EventId, MilliSecondsSinceUnixEpoch, OwnedUserId, UserId,
    };

    use similar_asserts::assert_eq;

    use super::{Room, RoomState};
    use crate::store::MemoryStore;

    fn make_room_test_helper(room_type: RoomState) -> (Arc<MemoryStore>, Room) {
        let store = Arc::new(MemoryStore::new());
        let user_id = user_id!("@me:example.org");
        let room_id = room_id!("!test:localhost");
        let (sender, _receiver) = tokio::sync::broadcast::channel(1);

        (store.clone(), Room::new(user_id, store, room_id, room_type, sender))
    }

    fn timestamp(minutes_ago: u32) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch::from_system_time(
            SystemTime::now().sub(Duration::from_secs((60 * minutes_ago).into())),
        )
        .expect("date out of range")
    }

    fn legacy_membership_for_my_call(
        device_id: &DeviceId,
        membership_id: &str,
        minutes_ago: u32,
    ) -> LegacyMembershipData {
        let (application, foci) = foci_and_application();
        assign!(
            LegacyMembershipData::from(LegacyMembershipDataInit {
                application,
                device_id: device_id.to_owned(),
                expires: Duration::from_millis(3_600_000),
                foci_active: foci,
                membership_id: membership_id.to_owned(),
            }),
            { created_ts: Some(timestamp(minutes_ago)) }
        )
    }

    fn legacy_member_state_event(
        memberships: Vec<LegacyMembershipData>,
        ev_id: &EventId,
        user_id: &UserId,
    ) -> AnySyncStateEvent {
        let content = CallMemberEventContent::new_legacy(memberships);

        AnySyncStateEvent::CallMember(SyncStateEvent::Original(OriginalSyncCallMemberEvent {
            content,
            event_id: ev_id.to_owned(),
            sender: user_id.to_owned(),
            // we can simply use now here since this will be dropped when using a MinimalStateEvent
            // in the roomInfo
            origin_server_ts: timestamp(0),
            state_key: CallMemberStateKey::new(user_id.to_owned(), None, false),
            unsigned: StateUnsigned::new(),
        }))
    }

    struct InitData<'a> {
        device_id: &'a DeviceId,
        minutes_ago: u32,
    }

    fn session_member_state_event(
        ev_id: &EventId,
        user_id: &UserId,
        init_data: Option<InitData<'_>>,
    ) -> AnySyncStateEvent {
        let application = Application::Call(CallApplicationContent::new(
            "my_call_id_1".to_owned(),
            ruma::events::call::member::CallScope::Room,
        ));
        let foci_preferred = vec![Focus::Livekit(LivekitFocus::new(
            "my_call_foci_alias".to_owned(),
            "https://lk.org".to_owned(),
        ))];
        let focus_active = ActiveFocus::Livekit(ActiveLivekitFocus::new());
        let (content, state_key) = match init_data {
            Some(InitData { device_id, minutes_ago }) => (
                CallMemberEventContent::new(
                    application,
                    device_id.to_owned(),
                    focus_active,
                    foci_preferred,
                    Some(timestamp(minutes_ago)),
                ),
                CallMemberStateKey::new(user_id.to_owned(), Some(device_id.to_owned()), false),
            ),
            None => (
                CallMemberEventContent::new_empty(None),
                CallMemberStateKey::new(user_id.to_owned(), None, false),
            ),
        };

        AnySyncStateEvent::CallMember(SyncStateEvent::Original(OriginalSyncCallMemberEvent {
            content,
            event_id: ev_id.to_owned(),
            sender: user_id.to_owned(),
            // we can simply use now here since this will be dropped when using a MinimalStateEvent
            // in the roomInfo
            origin_server_ts: timestamp(0),
            state_key,
            unsigned: StateUnsigned::new(),
        }))
    }

    fn foci_and_application() -> (Application, Vec<Focus>) {
        (
            Application::Call(CallApplicationContent::new(
                "my_call_id_1".to_owned(),
                ruma::events::call::member::CallScope::Room,
            )),
            vec![Focus::Livekit(LivekitFocus::new(
                "my_call_foci_alias".to_owned(),
                "https://lk.org".to_owned(),
            ))],
        )
    }

    fn receive_state_events(room: &Room, events: Vec<&AnySyncStateEvent>) {
        room.inner.update_if(|info| {
            let mut res = false;
            for ev in events {
                res |= info.handle_state_event(ev);
            }
            res
        });
    }

    /// `user_a`: empty memberships
    /// `user_b`: one membership
    /// `user_c`: two memberships (two devices)
    fn legacy_create_call_with_member_events_for_user(a: &UserId, b: &UserId, c: &UserId) -> Room {
        let (_, room) = make_room_test_helper(RoomState::Joined);

        let a_empty = legacy_member_state_event(Vec::new(), event_id!("$1234"), a);

        // make b 10min old
        let m_init_b = legacy_membership_for_my_call(device_id!("DEVICE_0"), "0", 1);
        let b_one = legacy_member_state_event(vec![m_init_b], event_id!("$12345"), b);

        // c1 1min old
        let m_init_c1 = legacy_membership_for_my_call(device_id!("DEVICE_0"), "0", 10);
        // c2 20min old
        let m_init_c2 = legacy_membership_for_my_call(device_id!("DEVICE_1"), "0", 20);
        let c_two = legacy_member_state_event(vec![m_init_c1, m_init_c2], event_id!("$123456"), c);

        // Intentionally use a non time sorted receive order.
        receive_state_events(&room, vec![&c_two, &a_empty, &b_one]);

        room
    }

    /// `user_a`: empty memberships
    /// `user_b`: one membership
    /// `user_c`: two memberships (two devices)
    fn session_create_call_with_member_events_for_user(a: &UserId, b: &UserId, c: &UserId) -> Room {
        let (_, room) = make_room_test_helper(RoomState::Joined);

        let a_empty = session_member_state_event(event_id!("$1234"), a, None);

        // make b 10min old
        let b_one = session_member_state_event(
            event_id!("$12345"),
            b,
            Some(InitData { device_id: "DEVICE_0".into(), minutes_ago: 1 }),
        );

        let m_c1 = session_member_state_event(
            event_id!("$123456_0"),
            c,
            Some(InitData { device_id: "DEVICE_0".into(), minutes_ago: 10 }),
        );
        let m_c2 = session_member_state_event(
            event_id!("$123456_1"),
            c,
            Some(InitData { device_id: "DEVICE_1".into(), minutes_ago: 20 }),
        );
        // Intentionally use a non time sorted receive order1
        receive_state_events(&room, vec![&m_c1, &m_c2, &a_empty, &b_one]);

        room
    }

    #[test]
    fn test_show_correct_active_call_state() {
        let room_legacy = legacy_create_call_with_member_events_for_user(&ALICE, &BOB, &CAROL);

        // This check also tests the ordering.
        // We want older events to be in the front.
        // user_b (Bob) is 1min old, c1 (CAROL) 10min old, c2 (CAROL) 20min old
        assert_eq!(
            vec![CAROL.to_owned(), CAROL.to_owned(), BOB.to_owned()],
            room_legacy.active_room_call_participants()
        );
        assert!(room_legacy.has_active_room_call());

        let room_session = session_create_call_with_member_events_for_user(&ALICE, &BOB, &CAROL);
        assert_eq!(
            vec![CAROL.to_owned(), CAROL.to_owned(), BOB.to_owned()],
            room_session.active_room_call_participants()
        );
        assert!(room_session.has_active_room_call());
    }

    #[test]
    fn test_active_call_is_false_when_everyone_left() {
        let room = legacy_create_call_with_member_events_for_user(&ALICE, &BOB, &CAROL);

        let b_empty_membership = legacy_member_state_event(Vec::new(), event_id!("$1234_1"), &BOB);
        let c_empty_membership =
            legacy_member_state_event(Vec::new(), event_id!("$12345_1"), &CAROL);

        receive_state_events(&room, vec![&b_empty_membership, &c_empty_membership]);

        // We have no active call anymore after emptying the memberships
        assert_eq!(Vec::<OwnedUserId>::new(), room.active_room_call_participants());
        assert!(!room.has_active_room_call());
    }
}

/// The content of an `m.room.create` event, with a required `creator` field.
///
/// Starting with room version 11, the `creator` field should be removed and the
/// `sender` field of the event should be used instead. This is reflected on
/// [`RoomCreateEventContent`].
///
/// This type was created as an alternative for ease of use. When it is used in
/// the SDK, it is constructed by copying the `sender` of the original event as
/// the `creator`.
#[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "m.room.create", kind = State, state_key_type = EmptyStateKey, custom_redacted)]
pub struct RoomCreateWithCreatorEventContent {
    /// The `user_id` of the room creator.
    ///
    /// This is set by the homeserver.
    ///
    /// While this should be optional since room version 11, we copy the sender
    /// of the event so we can still access it.
    pub creator: OwnedUserId,

    /// Whether or not this room's data should be transferred to other
    /// homeservers.
    #[serde(
        rename = "m.federate",
        default = "ruma::serde::default_true",
        skip_serializing_if = "ruma::serde::is_true"
    )]
    pub federate: bool,

    /// The version of the room.
    ///
    /// Defaults to `RoomVersionId::V1`.
    #[serde(default = "default_create_room_version_id")]
    pub room_version: RoomVersionId,

    /// A reference to the room this room replaces, if the previous room was
    /// upgraded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predecessor: Option<PreviousRoom>,

    /// The room type.
    ///
    /// This is currently only used for spaces.
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    pub room_type: Option<RoomType>,
}

impl RoomCreateWithCreatorEventContent {
    /// Constructs a `RoomCreateWithCreatorEventContent` with the given original
    /// content and sender.
    pub fn from_event_content(content: RoomCreateEventContent, sender: OwnedUserId) -> Self {
        let RoomCreateEventContent { federate, room_version, predecessor, room_type, .. } = content;
        Self { creator: sender, federate, room_version, predecessor, room_type }
    }

    fn into_event_content(self) -> (RoomCreateEventContent, OwnedUserId) {
        let Self { creator, federate, room_version, predecessor, room_type } = self;

        #[allow(deprecated)]
        let content = assign!(RoomCreateEventContent::new_v11(), {
            creator: Some(creator.clone()),
            federate,
            room_version,
            predecessor,
            room_type,
        });

        (content, creator)
    }
}

/// Redacted form of [`RoomCreateWithCreatorEventContent`].
pub type RedactedRoomCreateWithCreatorEventContent = RoomCreateWithCreatorEventContent;

impl RedactedStateEventContent for RedactedRoomCreateWithCreatorEventContent {
    type StateKey = EmptyStateKey;
}

impl RedactContent for RoomCreateWithCreatorEventContent {
    type Redacted = RedactedRoomCreateWithCreatorEventContent;

    fn redact(self, version: &RoomVersionId) -> Self::Redacted {
        let (content, sender) = self.into_event_content();
        // Use Ruma's redaction algorithm.
        let content = content.redact(version);
        Self::from_event_content(content, sender)
    }
}

fn default_create_room_version_id() -> RoomVersionId {
    RoomVersionId::V1
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
