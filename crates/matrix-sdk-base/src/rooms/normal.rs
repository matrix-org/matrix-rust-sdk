// Copyright 2020 The Matrix.org Foundation C.I.C.
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

#[cfg(feature = "e2e-encryption")]
use std::sync::RwLock as SyncRwLock;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    mem,
    sync::Arc,
};

use as_variant::as_variant;
use bitflags::bitflags;
use eyeball::{AsyncLock, ObservableWriteGuard, SharedObservable};
use futures_util::{Stream, StreamExt};
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_common::ring_buffer::RingBuffer;
#[cfg(feature = "e2e-encryption")]
use ruma::{events::AnySyncTimelineEvent, serde::Raw};
use ruma::{
    events::{
        direct::OwnedDirectUserIdentifier,
        ignored_user_list::IgnoredUserListEventContent,
        member_hints::MemberHintsEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::{
            avatar::{self},
            encryption::RoomEncryptionEventContent,
            guest_access::GuestAccess,
            history_visibility::HistoryVisibility,
            join_rules::JoinRule,
            member::{MembershipState, RoomMemberEventContent},
            power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
            tombstone::RoomTombstoneEventContent,
        },
        tag::Tags,
        AnyRoomAccountDataEvent, RoomAccountDataEventType, StateEventType, SyncStateEvent,
    },
    room::RoomType,
    EventId, OwnedEventId, OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomId, UserId,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, info, instrument, warn};

use super::{
    members::MemberRoomInfo, RoomCreateWithCreatorEventContent, RoomHero, RoomInfo,
    RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons, RoomMember, RoomNotableTags, SyncInfo,
};
use crate::{
    deserialized_responses::{DisplayName, MemberEvent, RawMemberEvent, SyncOrStrippedState},
    latest_event::LatestEvent,
    notification_settings::RoomNotificationMode,
    read_receipts::RoomReadReceipts,
    store::{DynStateStore, Result as StoreResult, StateStoreExt},
    sync::UnreadNotificationsCount,
    Error, MinimalStateEvent, RoomMemberships, StateStoreDataKey, StateStoreDataValue, StoreError,
};

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

/// Enum keeping track in which state the room is, e.g. if our own user is
/// joined, RoomState::Invited, or has left the room.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RoomState {
    /// The room is in a joined state.
    Joined,
    /// The room is in a left state.
    Left,
    /// The room is in an invited state.
    Invited,
    /// The room is in a knocked state.
    Knocked,
    /// The room is in a banned state.
    Banned,
}

impl From<&MembershipState> for RoomState {
    fn from(membership_state: &MembershipState) -> Self {
        match membership_state {
            MembershipState::Ban => Self::Banned,
            MembershipState::Invite => Self::Invited,
            MembershipState::Join => Self::Joined,
            MembershipState::Knock => Self::Knocked,
            MembershipState::Leave => Self::Left,
            _ => panic!("Unexpected MembershipState: {}", membership_state),
        }
    }
}

/// The kind of room member updates that just happened.
#[derive(Debug, Clone)]
pub enum RoomMembersUpdate {
    /// The whole list room members was reloaded.
    FullReload,
    /// A few members were updated, their user ids are included.
    Partial(BTreeSet<OwnedUserId>),
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

    /// Get the state of the room.
    pub fn state(&self) -> RoomState {
        self.inner.read().room_state
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

    /// Check if the room has its members fully synced.
    ///
    /// Members might be missing if lazy member loading was enabled for the
    /// sync.
    ///
    /// Returns true if no members are missing, false otherwise.
    pub fn are_members_synced(&self) -> bool {
        self.inner.read().members_synced
    }

    /// Mark this Room as holding all member information.
    ///
    /// Useful in tests if we want to persuade the Room not to sync when asked
    /// about its members.
    #[cfg(feature = "testing")]
    pub fn mark_members_synced(&self) {
        self.inner.update(|info| {
            info.members_synced = true;
        });
    }

    /// Mark this Room as still missing member information.
    pub fn mark_members_missing(&self) {
        self.inner.update_if(|info| {
            // notify observable subscribers only if the previous value was false
            mem::replace(&mut info.members_synced, false)
        })
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

    /// Get the encryption state of this room.
    pub fn encryption_state(&self) -> EncryptionState {
        self.inner.read().encryption_state()
    }

    /// Get the `m.room.encryption` content that enabled end to end encryption
    /// in the room.
    pub fn encryption_settings(&self) -> Option<RoomEncryptionEventContent> {
        self.inner.read().base_info.encryption.clone()
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

    /// Return the last event in this room, if one has been cached during
    /// sliding sync.
    pub fn latest_event(&self) -> Option<LatestEvent> {
        self.inner.read().latest_event.as_deref().cloned()
    }

    /// Return the most recent few encrypted events. When the keys come through
    /// to decrypt these, the most recent relevant one will replace
    /// latest_event. (We can't tell which one is relevant until
    /// they are decrypted.)
    #[cfg(feature = "e2e-encryption")]
    pub(crate) fn latest_encrypted_events(&self) -> Vec<Raw<AnySyncTimelineEvent>> {
        self.latest_encrypted_events.read().unwrap().iter().cloned().collect()
    }

    /// Replace our latest_event with the supplied event, and delete it and all
    /// older encrypted events from latest_encrypted_events, given that the
    /// new event was at the supplied index in the latest_encrypted_events
    /// list.
    ///
    /// Panics if index is not a valid index in the latest_encrypted_events
    /// list.
    ///
    /// It is the responsibility of the caller to apply the changes into the
    /// state store after calling this function.
    #[cfg(feature = "e2e-encryption")]
    pub(crate) fn on_latest_event_decrypted(
        &self,
        latest_event: Box<LatestEvent>,
        index: usize,
        changes: &mut crate::StateChanges,
        room_info_notable_updates: &mut BTreeMap<OwnedRoomId, RoomInfoNotableUpdateReasons>,
    ) {
        self.latest_encrypted_events.write().unwrap().drain(0..=index);

        let room_info = changes
            .room_infos
            .entry(self.room_id().to_owned())
            .or_insert_with(|| self.clone_info());

        room_info.latest_event = Some(latest_event);

        room_info_notable_updates
            .entry(self.room_id().to_owned())
            .or_default()
            .insert(RoomInfoNotableUpdateReasons::LATEST_EVENT);
    }

    /// Get the list of users ids that are considered to be joined members of
    /// this room.
    pub async fn joined_user_ids(&self) -> StoreResult<Vec<OwnedUserId>> {
        self.store.get_user_ids(self.room_id(), RoomMemberships::JOIN).await
    }

    /// Get the `RoomMember`s of this room that are known to the store, with the
    /// given memberships.
    pub async fn members(&self, memberships: RoomMemberships) -> StoreResult<Vec<RoomMember>> {
        let user_ids = self.store.get_user_ids(self.room_id(), memberships).await?;

        if user_ids.is_empty() {
            return Ok(Vec::new());
        }

        let member_events = self
            .store
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(
                self.room_id(),
                &user_ids,
            )
            .await?
            .into_iter()
            .map(|raw_event| raw_event.deserialize())
            .collect::<Result<Vec<_>, _>>()?;

        let mut profiles = self.store.get_profiles(self.room_id(), &user_ids).await?;

        let mut presences = self
            .store
            .get_presence_events(&user_ids)
            .await?
            .into_iter()
            .filter_map(|e| {
                e.deserialize().ok().map(|presence| (presence.sender.clone(), presence))
            })
            .collect::<BTreeMap<_, _>>();

        let display_names = member_events.iter().map(|e| e.display_name()).collect::<Vec<_>>();
        let room_info = self.member_room_info(&display_names).await?;

        let mut members = Vec::new();

        for event in member_events {
            let profile = profiles.remove(event.user_id());
            let presence = presences.remove(event.user_id());
            members.push(RoomMember::from_parts(event, profile, presence, &room_info))
        }

        Ok(members)
    }

    /// Get the heroes for this room.
    pub fn heroes(&self) -> Vec<RoomHero> {
        self.inner.read().heroes().to_vec()
    }

    /// Returns the number of members who have joined or been invited to the
    /// room.
    pub fn active_members_count(&self) -> u64 {
        self.inner.read().active_members_count()
    }

    /// Returns the number of members who have been invited to the room.
    pub fn invited_members_count(&self) -> u64 {
        self.inner.read().invited_members_count()
    }

    /// Returns the number of members who have joined the room.
    pub fn joined_members_count(&self) -> u64 {
        self.inner.read().joined_members_count()
    }

    /// Get the `RoomMember` with the given `user_id`.
    ///
    /// Returns `None` if the member was never part of this room, otherwise
    /// return a `RoomMember` that can be in a joined, RoomState::Invited, left,
    /// banned state.
    ///
    /// Async because it can read from storage.
    pub async fn get_member(&self, user_id: &UserId) -> StoreResult<Option<RoomMember>> {
        let Some(raw_event) = self.store.get_member_event(self.room_id(), user_id).await? else {
            debug!(%user_id, "Member event not found in state store");
            return Ok(None);
        };

        let event = raw_event.deserialize()?;

        let presence =
            self.store.get_presence_event(user_id).await?.and_then(|e| e.deserialize().ok());

        let profile = self.store.get_profile(self.room_id(), user_id).await?;

        let display_names = [event.display_name()];
        let room_info = self.member_room_info(&display_names).await?;

        Ok(Some(RoomMember::from_parts(event, profile, presence, &room_info)))
    }

    /// The current `MemberRoomInfo` for this room.
    ///
    /// Async because it can read from storage.
    async fn member_room_info<'a>(
        &self,
        display_names: &'a [DisplayName],
    ) -> StoreResult<MemberRoomInfo<'a>> {
        let max_power_level = self.max_power_level();
        let room_creator = self.inner.read().creator().map(ToOwned::to_owned);

        let power_levels = self
            .store
            .get_state_event_static(self.room_id())
            .await?
            .and_then(|e| e.deserialize().ok());

        let users_display_names =
            self.store.get_users_with_display_names(self.room_id(), display_names).await?;

        let ignored_users = self
            .store
            .get_account_data_event_static::<IgnoredUserListEventContent>()
            .await?
            .map(|c| c.deserialize())
            .transpose()?
            .map(|e| e.content.ignored_users.into_keys().collect());

        Ok(MemberRoomInfo {
            power_levels: power_levels.into(),
            max_power_level,
            room_creator,
            users_display_names,
            ignored_users,
        })
    }

    /// Get the `Tags` for this room.
    pub async fn tags(&self) -> StoreResult<Option<Tags>> {
        if let Some(AnyRoomAccountDataEvent::Tag(event)) = self
            .store
            .get_room_account_data_event(self.room_id(), RoomAccountDataEventType::Tag)
            .await?
            .and_then(|r| r.deserialize().ok())
        {
            Ok(Some(event.content.tags))
        } else {
            Ok(None)
        }
    }

    /// Check whether the room is marked as favourite.
    ///
    /// A room is considered favourite if it has received the `m.favourite` tag.
    pub fn is_favourite(&self) -> bool {
        self.inner.read().base_info.notable_tags.contains(RoomNotableTags::FAVOURITE)
    }

    /// Check whether the room is marked as low priority.
    ///
    /// A room is considered low priority if it has received the `m.lowpriority`
    /// tag.
    pub fn is_low_priority(&self) -> bool {
        self.inner.read().base_info.notable_tags.contains(RoomNotableTags::LOW_PRIORITY)
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

bitflags! {
    /// Room state filter as a bitset.
    ///
    /// Note that [`RoomStateFilter::empty()`] doesn't filter the results and
    /// is equivalent to [`RoomStateFilter::all()`].
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct RoomStateFilter: u16 {
        /// The room is in a joined state.
        const JOINED   = 0b00000001;
        /// The room is in an invited state.
        const INVITED  = 0b00000010;
        /// The room is in a left state.
        const LEFT     = 0b00000100;
        /// The room is in a knocked state.
        const KNOCKED  = 0b00001000;
        /// The room is in a banned state.
        const BANNED   = 0b00010000;
    }
}

impl RoomStateFilter {
    /// Whether the given room state matches this `RoomStateFilter`.
    pub fn matches(&self, state: RoomState) -> bool {
        if self.is_empty() {
            return true;
        }

        let bit_state = match state {
            RoomState::Joined => Self::JOINED,
            RoomState::Left => Self::LEFT,
            RoomState::Invited => Self::INVITED,
            RoomState::Knocked => Self::KNOCKED,
            RoomState::Banned => Self::BANNED,
        };

        self.contains(bit_state)
    }

    /// Get this `RoomStateFilter` as a list of matching [`RoomState`]s.
    pub fn as_vec(&self) -> Vec<RoomState> {
        let mut states = Vec::new();

        if self.contains(Self::JOINED) {
            states.push(RoomState::Joined);
        }
        if self.contains(Self::LEFT) {
            states.push(RoomState::Left);
        }
        if self.contains(Self::INVITED) {
            states.push(RoomState::Invited);
        }
        if self.contains(Self::KNOCKED) {
            states.push(RoomState::Knocked);
        }
        if self.contains(Self::BANNED) {
            states.push(RoomState::Banned);
        }

        states
    }
}

/// Represents the state of a room encryption.
#[derive(Debug)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum EncryptionState {
    /// The room is encrypted.
    Encrypted,

    /// The room is not encrypted.
    NotEncrypted,

    /// The state of the room encryption is unknown, probably because the
    /// `/sync` did not provide all data needed to decide.
    Unknown,
}

impl EncryptionState {
    /// Check whether `EncryptionState` is [`Encrypted`][Self::Encrypted].
    pub fn is_encrypted(&self) -> bool {
        matches!(self, Self::Encrypted)
    }

    /// Check whether `EncryptionState` is [`Unknown`][Self::Unknown].
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ops::{Not, Sub},
        str::FromStr,
        sync::Arc,
        time::Duration,
    };

    use assert_matches::assert_matches;
    use assign::assign;
    use matrix_sdk_common::deserialized_responses::TimelineEvent;
    use matrix_sdk_test::{async_test, ALICE, BOB, CAROL};
    use ruma::{
        device_id, event_id,
        events::{
            call::member::{
                ActiveFocus, ActiveLivekitFocus, Application, CallApplicationContent,
                CallMemberEventContent, CallMemberStateKey, Focus, LegacyMembershipData,
                LegacyMembershipDataInit, LivekitFocus, OriginalSyncCallMemberEvent,
            },
            room::encryption::{OriginalSyncRoomEncryptionEvent, RoomEncryptionEventContent},
            AnySyncStateEvent, EmptyStateKey, StateUnsigned, SyncStateEvent,
        },
        owned_room_id, room_id,
        serde::Raw,
        time::SystemTime,
        user_id, DeviceId, EventEncryptionAlgorithm, EventId, MilliSecondsSinceUnixEpoch,
        OwnedEventId, OwnedUserId, UserId,
    };
    use serde_json::json;
    use similar_asserts::assert_eq;
    use stream_assert::{assert_pending, assert_ready};

    use super::{EncryptionState, Room, RoomState};
    use crate::{
        latest_event::LatestEvent,
        response_processors as processors,
        store::{MemoryStore, RoomLoadSettings, StateChanges, StoreConfig},
        test_utils::logged_in_base_client,
        BaseClient, RoomStateFilter, SessionMeta,
    };

    #[async_test]
    async fn test_is_favourite() {
        // Given a room,
        let client =
            BaseClient::new(StoreConfig::new("cross-process-store-locks-holder-name".to_owned()));

        client
            .activate(
                SessionMeta {
                    user_id: user_id!("@alice:example.org").into(),
                    device_id: ruma::device_id!("AYEAYEAYE").into(),
                },
                RoomLoadSettings::default(),
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await
            .unwrap();

        let room_id = room_id!("!test:localhost");
        let room = client.get_or_create_room(room_id, RoomState::Joined);

        // Sanity checks to ensure the room isn't marked as favourite.
        assert!(room.is_favourite().not());

        // Subscribe to the `RoomInfo`.
        let mut room_info_subscriber = room.subscribe_info();

        assert_pending!(room_info_subscriber);

        // Create the tag.
        let tag_raw = Raw::new(&json!({
            "content": {
                "tags": {
                    "m.favourite": {
                        "order": 0.0
                    },
                },
            },
            "type": "m.tag",
        }))
        .unwrap()
        .cast();

        // When the new tag is handled and applied.
        let mut context = processors::Context::default();

        processors::account_data::for_room(&mut context, room_id, &[tag_raw], &client.state_store)
            .await;

        processors::changes::save_and_apply(
            context.clone(),
            &client.state_store,
            &client.ignore_user_list_changes,
            None,
        )
        .await
        .unwrap();

        // The `RoomInfo` is getting notified.
        assert_ready!(room_info_subscriber);
        assert_pending!(room_info_subscriber);

        // The room is now marked as favourite.
        assert!(room.is_favourite());

        // Now, let's remove the tag.
        let tag_raw = Raw::new(&json!({
            "content": {
                "tags": {},
            },
            "type": "m.tag"
        }))
        .unwrap()
        .cast();

        processors::account_data::for_room(&mut context, room_id, &[tag_raw], &client.state_store)
            .await;

        processors::changes::save_and_apply(
            context,
            &client.state_store,
            &client.ignore_user_list_changes,
            None,
        )
        .await
        .unwrap();

        // The `RoomInfo` is getting notified.
        assert_ready!(room_info_subscriber);
        assert_pending!(room_info_subscriber);

        // The room is now marked as _not_ favourite.
        assert!(room.is_favourite().not());
    }

    #[async_test]
    async fn test_is_low_priority() {
        // Given a room,
        let client =
            BaseClient::new(StoreConfig::new("cross-process-store-locks-holder-name".to_owned()));

        client
            .activate(
                SessionMeta {
                    user_id: user_id!("@alice:example.org").into(),
                    device_id: ruma::device_id!("AYEAYEAYE").into(),
                },
                RoomLoadSettings::default(),
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await
            .unwrap();

        let room_id = room_id!("!test:localhost");
        let room = client.get_or_create_room(room_id, RoomState::Joined);

        // Sanity checks to ensure the room isn't marked as low priority.
        assert!(!room.is_low_priority());

        // Subscribe to the `RoomInfo`.
        let mut room_info_subscriber = room.subscribe_info();

        assert_pending!(room_info_subscriber);

        // Create the tag.
        let tag_raw = Raw::new(&json!({
            "content": {
                "tags": {
                    "m.lowpriority": {
                        "order": 0.0
                    },
                }
            },
            "type": "m.tag"
        }))
        .unwrap()
        .cast();

        // When the new tag is handled and applied.
        let mut context = processors::Context::default();

        processors::account_data::for_room(&mut context, room_id, &[tag_raw], &client.state_store)
            .await;

        processors::changes::save_and_apply(
            context.clone(),
            &client.state_store,
            &client.ignore_user_list_changes,
            None,
        )
        .await
        .unwrap();

        // The `RoomInfo` is getting notified.
        assert_ready!(room_info_subscriber);
        assert_pending!(room_info_subscriber);

        // The room is now marked as low priority.
        assert!(room.is_low_priority());

        // Now, let's remove the tag.
        let tag_raw = Raw::new(&json!({
            "content": {
                "tags": {},
            },
            "type": "m.tag"
        }))
        .unwrap()
        .cast();

        processors::account_data::for_room(&mut context, room_id, &[tag_raw], &client.state_store)
            .await;

        processors::changes::save_and_apply(
            context,
            &client.state_store,
            &client.ignore_user_list_changes,
            None,
        )
        .await
        .unwrap();

        // The `RoomInfo` is getting notified.
        assert_ready!(room_info_subscriber);
        assert_pending!(room_info_subscriber);

        // The room is now marked as _not_ low priority.
        assert!(room.is_low_priority().not());
    }

    fn make_room_test_helper(room_type: RoomState) -> (Arc<MemoryStore>, Room) {
        let store = Arc::new(MemoryStore::new());
        let user_id = user_id!("@me:example.org");
        let room_id = room_id!("!test:localhost");
        let (sender, _receiver) = tokio::sync::broadcast::channel(1);

        (store.clone(), Room::new(user_id, store, room_id, room_type, sender))
    }

    #[cfg(feature = "e2e-encryption")]
    #[async_test]
    async fn test_setting_the_latest_event_doesnt_cause_a_room_info_notable_update() {
        use assert_matches::assert_matches;

        use crate::{RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons};

        // Given a room,
        let client =
            BaseClient::new(StoreConfig::new("cross-process-store-locks-holder-name".to_owned()));

        client
            .activate(
                SessionMeta {
                    user_id: user_id!("@alice:example.org").into(),
                    device_id: ruma::device_id!("AYEAYEAYE").into(),
                },
                RoomLoadSettings::default(),
                None,
            )
            .await
            .unwrap();

        let room_id = room_id!("!test:localhost");
        let room = client.get_or_create_room(room_id, RoomState::Joined);

        // That has an encrypted event,
        add_encrypted_event(&room, "$A");
        // Sanity: it has no latest_event
        assert!(room.latest_event().is_none());

        // When I set up an observer on the latest_event,
        let mut room_info_notable_update = client.room_info_notable_update_receiver();

        // And I provide a decrypted event to replace the encrypted one,
        let event = make_latest_event("$A");

        let mut context = processors::Context::default();
        room.on_latest_event_decrypted(
            event.clone(),
            0,
            &mut context.state_changes,
            &mut context.room_info_notable_updates,
        );

        assert!(context.room_info_notable_updates.contains_key(room_id));

        // The subscriber isn't notified at this point.
        assert!(room_info_notable_update.is_empty());

        // Then updating the room info will store the event,
        processors::changes::save_and_apply(
            context,
            &client.state_store,
            &client.ignore_user_list_changes,
            None,
        )
        .await
        .unwrap();

        assert_eq!(room.latest_event().unwrap().event_id(), event.event_id());

        // And wake up the subscriber.
        assert_matches!(
            room_info_notable_update.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(reasons.contains(RoomInfoNotableUpdateReasons::LATEST_EVENT));
            }
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[async_test]
    async fn test_when_we_provide_a_newly_decrypted_event_it_replaces_latest_event() {
        use std::collections::BTreeMap;

        // Given a room with an encrypted event
        let (_store, room) = make_room_test_helper(RoomState::Joined);
        add_encrypted_event(&room, "$A");
        // Sanity: it has no latest_event
        assert!(room.latest_event().is_none());

        // When I provide a decrypted event to replace the encrypted one
        let event = make_latest_event("$A");
        let mut changes = StateChanges::default();
        let mut room_info_notable_updates = BTreeMap::new();
        room.on_latest_event_decrypted(
            event.clone(),
            0,
            &mut changes,
            &mut room_info_notable_updates,
        );
        room.set_room_info(
            changes.room_infos.get(room.room_id()).cloned().unwrap(),
            room_info_notable_updates.get(room.room_id()).copied().unwrap(),
        );

        // Then is it stored
        assert_eq!(room.latest_event().unwrap().event_id(), event.event_id());
    }

    #[cfg(feature = "e2e-encryption")]
    #[async_test]
    async fn test_when_a_newly_decrypted_event_appears_we_delete_all_older_encrypted_events() {
        use std::collections::BTreeMap;

        // Given a room with some encrypted events and a latest event
        let (_store, room) = make_room_test_helper(RoomState::Joined);
        room.inner.update(|info| info.latest_event = Some(make_latest_event("$A")));
        add_encrypted_event(&room, "$0");
        add_encrypted_event(&room, "$1");
        add_encrypted_event(&room, "$2");
        add_encrypted_event(&room, "$3");

        // When I provide a latest event
        let new_event = make_latest_event("$1");
        let new_event_index = 1;
        let mut changes = StateChanges::default();
        let mut room_info_notable_updates = BTreeMap::new();
        room.on_latest_event_decrypted(
            new_event.clone(),
            new_event_index,
            &mut changes,
            &mut room_info_notable_updates,
        );
        room.set_room_info(
            changes.room_infos.get(room.room_id()).cloned().unwrap(),
            room_info_notable_updates.get(room.room_id()).copied().unwrap(),
        );

        // Then the encrypted events list is shortened to only newer events
        let enc_evs = room.latest_encrypted_events();
        assert_eq!(enc_evs.len(), 2);
        assert_eq!(enc_evs[0].get_field::<&str>("event_id").unwrap().unwrap(), "$2");
        assert_eq!(enc_evs[1].get_field::<&str>("event_id").unwrap().unwrap(), "$3");

        // And the event is stored
        assert_eq!(room.latest_event().unwrap().event_id(), new_event.event_id());
    }

    #[cfg(feature = "e2e-encryption")]
    #[async_test]
    async fn test_replacing_the_newest_event_leaves_none_left() {
        use std::collections::BTreeMap;

        // Given a room with some encrypted events
        let (_store, room) = make_room_test_helper(RoomState::Joined);
        add_encrypted_event(&room, "$0");
        add_encrypted_event(&room, "$1");
        add_encrypted_event(&room, "$2");
        add_encrypted_event(&room, "$3");

        // When I provide a latest event and say it was the very latest
        let new_event = make_latest_event("$3");
        let new_event_index = 3;
        let mut changes = StateChanges::default();
        let mut room_info_notable_updates = BTreeMap::new();
        room.on_latest_event_decrypted(
            new_event,
            new_event_index,
            &mut changes,
            &mut room_info_notable_updates,
        );
        room.set_room_info(
            changes.room_infos.get(room.room_id()).cloned().unwrap(),
            room_info_notable_updates.get(room.room_id()).copied().unwrap(),
        );

        // Then the encrypted events list ie empty
        let enc_evs = room.latest_encrypted_events();
        assert_eq!(enc_evs.len(), 0);
    }

    #[cfg(feature = "e2e-encryption")]
    fn add_encrypted_event(room: &Room, event_id: &str) {
        room.latest_encrypted_events
            .write()
            .unwrap()
            .push(Raw::from_json_string(json!({ "event_id": event_id }).to_string()).unwrap());
    }

    #[cfg(feature = "e2e-encryption")]
    fn make_latest_event(event_id: &str) -> Box<LatestEvent> {
        Box::new(LatestEvent::new(TimelineEvent::new(
            Raw::from_json_string(json!({ "event_id": event_id }).to_string()).unwrap(),
        )))
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

    #[test]
    fn test_encryption_is_set_when_encryption_event_is_received_encrypted() {
        let (_store, room) = make_room_test_helper(RoomState::Joined);

        assert_matches!(room.encryption_state(), EncryptionState::Unknown);

        let encryption_content =
            RoomEncryptionEventContent::new(EventEncryptionAlgorithm::MegolmV1AesSha2);
        let encryption_event = AnySyncStateEvent::RoomEncryption(SyncStateEvent::Original(
            OriginalSyncRoomEncryptionEvent {
                content: encryption_content,
                event_id: OwnedEventId::from_str("$1234_1").unwrap(),
                sender: ALICE.to_owned(),
                // we can simply use now here since this will be dropped when using a
                // MinimalStateEvent in the roomInfo
                origin_server_ts: timestamp(0),
                state_key: EmptyStateKey,
                unsigned: StateUnsigned::new(),
            },
        ));
        receive_state_events(&room, vec![&encryption_event]);

        assert_matches!(room.encryption_state(), EncryptionState::Encrypted);
    }

    #[test]
    fn test_encryption_is_set_when_encryption_event_is_received_not_encrypted() {
        let (_store, room) = make_room_test_helper(RoomState::Joined);

        assert_matches!(room.encryption_state(), EncryptionState::Unknown);
        room.inner.update_if(|info| {
            info.mark_encryption_state_synced();

            false
        });

        assert_matches!(room.encryption_state(), EncryptionState::NotEncrypted);
    }

    #[test]
    fn test_encryption_state() {
        assert!(EncryptionState::Unknown.is_unknown());
        assert!(EncryptionState::Encrypted.is_unknown().not());
        assert!(EncryptionState::NotEncrypted.is_unknown().not());

        assert!(EncryptionState::Unknown.is_encrypted().not());
        assert!(EncryptionState::Encrypted.is_encrypted());
        assert!(EncryptionState::NotEncrypted.is_encrypted().not());
    }

    #[async_test]
    async fn test_room_state_filters() {
        let client = logged_in_base_client(None).await;

        let joined_room_id = owned_room_id!("!joined:example.org");
        client.get_or_create_room(&joined_room_id, RoomState::Joined);

        let invited_room_id = owned_room_id!("!invited:example.org");
        client.get_or_create_room(&invited_room_id, RoomState::Invited);

        let left_room_id = owned_room_id!("!left:example.org");
        client.get_or_create_room(&left_room_id, RoomState::Left);

        let knocked_room_id = owned_room_id!("!knocked:example.org");
        client.get_or_create_room(&knocked_room_id, RoomState::Knocked);

        let banned_room_id = owned_room_id!("!banned:example.org");
        client.get_or_create_room(&banned_room_id, RoomState::Banned);

        let joined_rooms = client.rooms_filtered(RoomStateFilter::JOINED);
        assert_eq!(joined_rooms.len(), 1);
        assert_eq!(joined_rooms[0].state(), RoomState::Joined);
        assert_eq!(joined_rooms[0].room_id, joined_room_id);

        let invited_rooms = client.rooms_filtered(RoomStateFilter::INVITED);
        assert_eq!(invited_rooms.len(), 1);
        assert_eq!(invited_rooms[0].state(), RoomState::Invited);
        assert_eq!(invited_rooms[0].room_id, invited_room_id);

        let left_rooms = client.rooms_filtered(RoomStateFilter::LEFT);
        assert_eq!(left_rooms.len(), 1);
        assert_eq!(left_rooms[0].state(), RoomState::Left);
        assert_eq!(left_rooms[0].room_id, left_room_id);

        let knocked_rooms = client.rooms_filtered(RoomStateFilter::KNOCKED);
        assert_eq!(knocked_rooms.len(), 1);
        assert_eq!(knocked_rooms[0].state(), RoomState::Knocked);
        assert_eq!(knocked_rooms[0].room_id, knocked_room_id);

        let banned_rooms = client.rooms_filtered(RoomStateFilter::BANNED);
        assert_eq!(banned_rooms.len(), 1);
        assert_eq!(banned_rooms[0].state(), RoomState::Banned);
        assert_eq!(banned_rooms[0].room_id, banned_room_id);
    }

    #[test]
    fn test_room_state_filters_as_vec() {
        assert_eq!(RoomStateFilter::JOINED.as_vec(), vec![RoomState::Joined]);
        assert_eq!(RoomStateFilter::LEFT.as_vec(), vec![RoomState::Left]);
        assert_eq!(RoomStateFilter::INVITED.as_vec(), vec![RoomState::Invited]);
        assert_eq!(RoomStateFilter::KNOCKED.as_vec(), vec![RoomState::Knocked]);
        assert_eq!(RoomStateFilter::BANNED.as_vec(), vec![RoomState::Banned]);

        // Check all filters are taken into account
        assert_eq!(
            RoomStateFilter::all().as_vec(),
            vec![
                RoomState::Joined,
                RoomState::Left,
                RoomState::Invited,
                RoomState::Knocked,
                RoomState::Banned
            ]
        );
    }
}
