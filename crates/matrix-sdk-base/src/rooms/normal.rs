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

use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, RwLock as SyncRwLock},
};

use bitflags::bitflags;
use futures_util::stream::{self, StreamExt};
#[cfg(feature = "experimental-sliding-sync")]
use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
#[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
use matrix_sdk_common::ring_buffer::RingBuffer;
use ruma::{
    api::client::sync::sync_events::v3::RoomSummary as RumaSummary,
    events::{
        ignored_user_list::IgnoredUserListEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::{
            create::RoomCreateEventContent,
            encryption::RoomEncryptionEventContent,
            guest_access::GuestAccess,
            history_visibility::HistoryVisibility,
            join_rules::JoinRule,
            member::{MembershipState, RoomMemberEventContent},
            name::RoomNameEventContent,
            redaction::OriginalSyncRoomRedactionEvent,
            tombstone::RoomTombstoneEventContent,
        },
        tag::Tags,
        AnyRoomAccountDataEvent, AnyStrippedStateEvent, AnySyncStateEvent,
        RoomAccountDataEventType,
    },
    room::RoomType,
    EventId, OwnedEventId, OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomAliasId,
    RoomId, RoomVersionId, UserId,
};
#[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
use ruma::{events::AnySyncTimelineEvent, serde::Raw};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, warn};

use super::{
    members::{MemberInfo, MemberRoomInfo},
    BaseRoomInfo, DisplayName, RoomMember,
};
use crate::{
    deserialized_responses::MemberEvent,
    store::{DynStateStore, Result as StoreResult, StateStoreExt},
    sync::UnreadNotificationsCount,
    MinimalStateEvent, OriginalMinimalStateEvent, RoomMemberships,
};

/// The underlying room data structure collecting state for joined, left and
/// invited rooms.
#[derive(Debug, Clone)]
pub struct Room {
    room_id: OwnedRoomId,
    own_user_id: OwnedUserId,
    inner: Arc<SyncRwLock<RoomInfo>>,
    store: Arc<DynStateStore>,

    /// The most recent few encrypted events. When the keys come through to
    /// decrypt these, the most recent relevant one will replace
    /// `latest_event`. (We can't tell which one is relevant until
    /// they are decrypted.)
    ///
    /// Currently, these are held in Room rather than RoomInfo, because we were
    /// not sure whether holding too many of them might make the cache too
    /// slow to load on startup. Keeping them here means they are not cached
    /// to disk but held in memory.
    #[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
    pub latest_encrypted_events: Arc<SyncRwLock<RingBuffer<Raw<AnySyncTimelineEvent>>>>,
}

/// The room summary containing member counts and members that should be used to
/// calculate the room display name.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoomSummary {
    /// The heroes of the room, members that should be used for the room display
    /// name.
    heroes: Vec<String>,
    /// The number of members that are considered to be joined to the room.
    joined_member_count: u64,
    /// The number of members that are considered to be invited to the room.
    invited_member_count: u64,
}

/// Enum keeping track in which state the room is, e.g. if our own user is
/// joined, invited, or has left the room.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RoomState {
    /// The room is in a joined state.
    Joined,
    /// The room is in a left state.
    Left,
    /// The room is in a invited state.
    Invited,
}

impl From<&MembershipState> for RoomState {
    fn from(membership_state: &MembershipState) -> Self {
        // We consider Ban, Knock and Leave to be Left, because they all mean we are not
        // in the room.
        match membership_state {
            MembershipState::Ban => Self::Left,
            MembershipState::Invite => Self::Invited,
            MembershipState::Join => Self::Joined,
            MembershipState::Knock => Self::Left,
            MembershipState::Leave => Self::Left,
            _ => panic!("Unexpected MembershipState: {}", membership_state),
        }
    }
}

impl Room {
    /// The size of the latest_encrypted_events RingBuffer
    #[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
    const MAX_ENCRYPTED_EVENTS: usize = 10;

    pub(crate) fn new(
        own_user_id: &UserId,
        store: Arc<DynStateStore>,
        room_id: &RoomId,
        room_state: RoomState,
    ) -> Self {
        let room_info = RoomInfo::new(room_id, room_state);
        Self::restore(own_user_id, store, room_info)
    }

    pub(crate) fn restore(
        own_user_id: &UserId,
        store: Arc<DynStateStore>,
        room_info: RoomInfo,
    ) -> Self {
        Self {
            own_user_id: own_user_id.into(),
            room_id: room_info.room_id.clone(),
            store,
            inner: Arc::new(SyncRwLock::new(room_info)),
            #[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
            latest_encrypted_events: Arc::new(SyncRwLock::new(RingBuffer::new(
                Self::MAX_ENCRYPTED_EVENTS,
            ))),
        }
    }

    /// Get the unique room id of the room.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Get our own user id.
    pub fn own_user_id(&self) -> &UserId {
        &self.own_user_id
    }

    /// Get the state of the room.
    pub fn state(&self) -> RoomState {
        self.inner.read().unwrap().room_state
    }

    /// Whether this room's [`RoomType`] is `m.space`.
    pub fn is_space(&self) -> bool {
        self.inner.read().unwrap().room_type().is_some_and(|t| *t == RoomType::Space)
    }

    /// Get the unread notification counts.
    pub fn unread_notification_counts(&self) -> UnreadNotificationsCount {
        self.inner.read().unwrap().notification_counts
    }

    /// Check if the room has its members fully synced.
    ///
    /// Members might be missing if lazy member loading was enabled for the
    /// sync.
    ///
    /// Returns true if no members are missing, false otherwise.
    pub fn are_members_synced(&self) -> bool {
        self.inner.read().unwrap().members_synced
    }

    /// Mark this Room as still missing member information.
    pub fn mark_members_missing(&self) {
        self.inner.write().unwrap().mark_members_missing()
    }

    /// Check if the room states have been synced
    ///
    /// States might be missing if we have only seen the room_id of this Room
    /// so far, for example as the response for a `create_room` request without
    /// being synced yet.
    ///
    /// Returns true if the state is fully synced, false otherwise.
    pub fn is_state_fully_synced(&self) -> bool {
        self.inner.read().unwrap().sync_info == SyncInfo::FullySynced
    }

    /// Check if the room has its encryption event synced.
    ///
    /// The encryption event can be missing when the room hasn't appeared in
    /// sync yet.
    ///
    /// Returns true if the encryption state is synced, false otherwise.
    pub fn is_encryption_state_synced(&self) -> bool {
        self.inner.read().unwrap().encryption_state_synced
    }

    /// Get the `prev_batch` token that was received from the last sync. May be
    /// `None` if the last sync contained the full room history.
    pub fn last_prev_batch(&self) -> Option<String> {
        self.inner.read().unwrap().last_prev_batch.clone()
    }

    /// Get the avatar url of this room.
    pub fn avatar_url(&self) -> Option<OwnedMxcUri> {
        self.inner
            .read()
            .unwrap()
            .base_info
            .avatar
            .as_ref()
            .and_then(|e| e.as_original().and_then(|e| e.content.url.clone()))
    }

    /// Get the canonical alias of this room.
    pub fn canonical_alias(&self) -> Option<OwnedRoomAliasId> {
        self.inner.read().unwrap().canonical_alias().map(ToOwned::to_owned)
    }

    /// Get the canonical alias of this room.
    pub fn alt_aliases(&self) -> Vec<OwnedRoomAliasId> {
        self.inner.read().unwrap().alt_aliases().to_owned()
    }

    /// Get the `m.room.create` content of this room.
    ///
    /// This usually isn't optional but some servers might not send an
    /// `m.room.create` event as the first event for a given room, thus this can
    /// be optional.
    ///
    /// It can also be redacted in current room versions, leaving only the
    /// `creator` field.
    pub fn create_content(&self) -> Option<RoomCreateEventContent> {
        self.inner
            .read()
            .unwrap()
            .base_info
            .create
            .as_ref()
            .and_then(|e| e.as_original().map(|e| e.content.clone()))
    }

    /// Is this room considered a direct message.
    #[instrument(skip_all, fields(room_id = ?self.room_id))]
    pub async fn is_direct(&self) -> StoreResult<bool> {
        match self.state() {
            RoomState::Joined | RoomState::Left => {
                Ok(!self.inner.read().unwrap().base_info.dm_targets.is_empty())
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
    pub fn direct_targets(&self) -> HashSet<OwnedUserId> {
        self.inner.read().unwrap().base_info.dm_targets.clone()
    }

    /// Is the room encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.inner.read().unwrap().is_encrypted()
    }

    /// Get the `m.room.encryption` content that enabled end to end encryption
    /// in the room.
    pub fn encryption_settings(&self) -> Option<RoomEncryptionEventContent> {
        self.inner.read().unwrap().base_info.encryption.clone()
    }

    /// Get the guest access policy of this room.
    pub fn guest_access(&self) -> GuestAccess {
        self.inner.read().unwrap().guest_access().clone()
    }

    /// Get the history visibility policy of this room.
    pub fn history_visibility(&self) -> HistoryVisibility {
        self.inner.read().unwrap().history_visibility().clone()
    }

    /// Is the room considered to be public.
    pub fn is_public(&self) -> bool {
        matches!(self.join_rule(), JoinRule::Public)
    }

    /// Get the join rule policy of this room.
    pub fn join_rule(&self) -> JoinRule {
        self.inner.read().unwrap().join_rule().clone()
    }

    /// Get the maximum power level that this room contains.
    ///
    /// This is useful if one wishes to normalize the power levels, e.g. from
    /// 0-100 where 100 would be the max power level.
    pub fn max_power_level(&self) -> i64 {
        self.inner.read().unwrap().base_info.max_power_level
    }

    /// Get the `m.room.name` of this room.
    pub fn name(&self) -> Option<String> {
        self.inner.read().unwrap().name().map(ToOwned::to_owned)
    }

    /// Has the room been tombstoned.
    pub fn is_tombstoned(&self) -> bool {
        self.inner.read().unwrap().base_info.tombstone.is_some()
    }

    /// Get the `m.room.tombstone` content of this room if there is one.
    pub fn tombstone(&self) -> Option<RoomTombstoneEventContent> {
        self.inner.read().unwrap().tombstone().cloned()
    }

    /// Get the topic of the room.
    pub fn topic(&self) -> Option<String> {
        self.inner.read().unwrap().topic().map(ToOwned::to_owned)
    }

    /// Return the cached display name of the room if it was provided via sync,
    /// or otherwise calculate it, taking into account its name, aliases and
    /// members.
    ///
    /// The display name is calculated according to [this algorithm][spec].
    ///
    /// [spec]: <https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room>
    pub async fn display_name(&self) -> StoreResult<DisplayName> {
        self.calculate_name().await
    }

    /// Return the last event in this room, if one has been cached during
    /// sliding sync.
    #[cfg(feature = "experimental-sliding-sync")]
    pub fn latest_event(&self) -> Option<SyncTimelineEvent> {
        self.inner.read().unwrap().latest_event.clone()
    }

    /// Update the last event in the room
    #[cfg(feature = "experimental-sliding-sync")]
    pub(crate) fn set_latest_event(&self, latest_event: Option<SyncTimelineEvent>) {
        self.inner.write().unwrap().latest_event = latest_event;
    }

    /// Return the most recent few encrypted events. When the keys come through
    /// to decrypt these, the most recent relevant one will replace
    /// latest_event. (We can't tell which one is relevant until
    /// they are decrypted.)
    #[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
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
    #[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
    pub(crate) fn on_latest_event_decrypted(&mut self, event: SyncTimelineEvent, index: usize) {
        self.set_latest_event(Some(event));
        self.latest_encrypted_events.write().unwrap().drain(0..=index);
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

        let display_names =
            member_events.iter().map(|e| e.display_name().to_owned()).collect::<Vec<_>>();
        let room_info = self.member_room_info(&display_names).await?;

        let mut members = Vec::new();

        for event in member_events {
            let profile = profiles.remove(event.user_id());
            let presence = presences.remove(event.user_id());

            let member_info = MemberInfo { event, profile, presence };

            members.push(RoomMember::from_parts(member_info, &room_info))
        }

        Ok(members)
    }

    /// Get the list of `RoomMember`s that are considered to be joined members
    /// of this room.
    #[deprecated = "Use members with RoomMemberships::JOIN instead"]
    pub async fn joined_members(&self) -> StoreResult<Vec<RoomMember>> {
        self.members(RoomMemberships::JOIN).await
    }

    /// Get the list of `RoomMember`s that are considered to be joined or
    /// invited members of this room.
    #[deprecated = "Use members with RoomMemberships::ACTIVE instead"]
    pub async fn active_members(&self) -> StoreResult<Vec<RoomMember>> {
        self.members(RoomMemberships::ACTIVE).await
    }

    /// Returns the number of members who have joined or been invited to the
    /// room.
    pub fn active_members_count(&self) -> u64 {
        self.inner.read().unwrap().active_members_count()
    }

    /// Returns the number of members who have been invited to the room.
    pub fn invited_members_count(&self) -> u64 {
        self.inner.read().unwrap().invited_members_count()
    }

    /// Returns the number of members who have joined the room.
    pub fn joined_members_count(&self) -> u64 {
        self.inner.read().unwrap().joined_members_count()
    }

    async fn calculate_name(&self) -> StoreResult<DisplayName> {
        let summary = {
            let inner = self.inner.read().unwrap();

            if let Some(name) = &inner.name() {
                let name = name.trim();
                return Ok(DisplayName::Named(name.to_owned()));
            } else if let Some(alias) = inner.canonical_alias() {
                let alias = alias.alias().trim();
                return Ok(DisplayName::Aliased(alias.to_owned()));
            }
            inner.summary.clone()
        };

        let is_own_member = |m: &RoomMember| m.user_id() == &*self.own_user_id;
        let is_own_user_id = |u: &str| u == self.own_user_id().as_str();

        let members: Vec<RoomMember> = if summary.heroes.is_empty() {
            self.members(RoomMemberships::ACTIVE)
                .await?
                .into_iter()
                .filter(|u| !is_own_member(u))
                .take(5)
                .collect()
        } else {
            let members: Vec<_> =
                stream::iter(summary.heroes.iter().filter(|u| !is_own_user_id(u)))
                    .filter_map(|u| async move {
                        let user_id = UserId::parse(u.as_str()).ok()?;
                        self.get_member(&user_id).await.transpose()
                    })
                    .collect()
                    .await;

            let members: StoreResult<Vec<_>> = members.into_iter().collect();

            members?
        };

        let (joined, invited) = match self.state() {
            RoomState::Invited => {
                // when we were invited we don't have a proper summary, we have to do best
                // guessing
                (members.len() as u64, 1u64)
            }
            RoomState::Joined if summary.joined_member_count == 0 => {
                // joined but the summary is not completed yet
                (
                    (members.len() as u64) + 1, // we've taken ourselves out of the count
                    summary.invited_member_count,
                )
            }
            _ => (summary.joined_member_count, summary.invited_member_count),
        };

        debug!(
            room_id = self.room_id().as_str(),
            own_user = self.own_user_id.as_str(),
            joined, invited,
            heroes = ?members,
            "Calculating name for a room",
        );

        let inner = self.inner.read().unwrap();
        Ok(inner.base_info.calculate_room_name(joined, invited, members))
    }

    /// Clone the inner RoomInfo
    pub fn clone_info(&self) -> RoomInfo {
        (*self.inner.read().unwrap()).clone()
    }

    /// Update the summary with given RoomInfo
    pub fn update_summary(&self, summary: RoomInfo) {
        let mut inner = self.inner.write().unwrap();
        *inner = summary;
    }

    /// Get the `RoomMember` with the given `user_id`.
    ///
    /// Returns `None` if the member was never part of this room, otherwise
    /// return a `RoomMember` that can be in a joined, invited, left, banned
    /// state.
    pub async fn get_member(&self, user_id: &UserId) -> StoreResult<Option<RoomMember>> {
        let Some(raw_event) = self.store.get_member_event(self.room_id(), user_id).await? else {
            return Ok(None);
        };
        let event = raw_event.deserialize()?;

        let presence =
            self.store.get_presence_event(user_id).await?.and_then(|e| e.deserialize().ok());
        let profile = self.store.get_profile(self.room_id(), user_id).await?;

        let display_names = [event.display_name().to_owned()];
        let room_info = self.member_room_info(&display_names).await?;

        let member_info = MemberInfo { event, profile, presence };

        Ok(Some(RoomMember::from_parts(member_info, &room_info)))
    }

    /// The current `MemberRoomInfo` for this room.
    async fn member_room_info<'a>(
        &self,
        display_names: &'a [String],
    ) -> StoreResult<MemberRoomInfo<'a>> {
        let max_power_level = self.max_power_level();
        let room_creator = self.inner.read().unwrap().creator().map(ToOwned::to_owned);

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

    /// Get the receipt as an `OwnedEventId` and `Receipt` tuple for the given
    /// `receipt_type`, `thread` and `user_id` in this room.
    pub async fn user_receipt(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> StoreResult<Option<(OwnedEventId, Receipt)>> {
        self.store.get_user_room_receipt_event(self.room_id(), receipt_type, thread, user_id).await
    }

    /// Get the receipts as a list of `OwnedUserId` and `Receipt` tuples for the
    /// given `receipt_type`, `thread` and `event_id` in this room.
    pub async fn event_receipts(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> StoreResult<Vec<(OwnedUserId, Receipt)>> {
        self.store
            .get_event_room_receipt_events(self.room_id(), receipt_type, thread, event_id)
            .await
    }
}

/// The underlying pure data structure for joined and left rooms.
///
/// Holds all the info needed to persist a room into the state store.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomInfo {
    /// The unique room id of the room.
    pub(crate) room_id: OwnedRoomId,
    /// The state of the room.
    #[serde(rename = "room_type")] // for backwards compatibility
    room_state: RoomState,
    /// The unread notifications counts.
    notification_counts: UnreadNotificationsCount,
    /// The summary of this room.
    summary: RoomSummary,
    /// Flag remembering if the room members are synced.
    members_synced: bool,
    /// The prev batch of this room we received during the last sync.
    pub(crate) last_prev_batch: Option<String>,
    /// How much we know about this room.
    #[serde(default = "SyncInfo::complete")] // see fn docs for why we use this default
    sync_info: SyncInfo,
    /// Whether or not the encryption info was been synced.
    #[serde(default = "encryption_state_default")] // see fn docs for why we use this default
    encryption_state_synced: bool,
    /// The last event send by sliding sync
    #[cfg(feature = "experimental-sliding-sync")]
    pub latest_event: Option<SyncTimelineEvent>,
    /// Base room info which holds some basic event contents important for the
    /// room state.
    pub(crate) base_info: BaseRoomInfo,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum SyncInfo {
    /// We only know the room exists and whether it is in invite / joined / left
    /// state.
    ///
    /// This is the case when we have a limited sync or only seen the room
    /// because of a request we've done, like a room creation event.
    NoState,

    /// Some states have been synced, but they might have been filtered or is
    /// stale, as it is from a room we've left.
    PartiallySynced,

    /// We have all the latest state events.
    FullySynced,
}

impl SyncInfo {
    // The sync_info field introduced a new field in the database schema, but to
    // avoid a database migration, we let serde assume that if the room is in
    // the database, yet the field isn't, we have synced it before this field
    // was introduced - which was a a full sync.
    fn complete() -> Self {
        SyncInfo::FullySynced
    }
}

// The encryption_state_synced field introduced a new field in the database
// schema, but to avoid a database migration, we let serde assume that if
// the room is in the database, yet the field isn't, we have synced it
// before this field was introduced - which was a a full sync.
fn encryption_state_default() -> bool {
    true
}

impl RoomInfo {
    #[doc(hidden)] // used by store tests, otherwise it would be pub(crate)
    pub fn new(room_id: &RoomId, room_state: RoomState) -> Self {
        Self {
            room_id: room_id.into(),
            room_state,
            notification_counts: Default::default(),
            summary: Default::default(),
            members_synced: false,
            last_prev_batch: None,
            sync_info: SyncInfo::NoState,
            encryption_state_synced: false,
            #[cfg(feature = "experimental-sliding-sync")]
            latest_event: None,
            base_info: BaseRoomInfo::new(),
        }
    }

    /// Mark this Room as joined.
    pub fn mark_as_joined(&mut self) {
        self.room_state = RoomState::Joined;
    }

    /// Mark this Room as left.
    pub fn mark_as_left(&mut self) {
        self.room_state = RoomState::Left;
    }

    /// Mark this Room as invited.
    pub fn mark_as_invited(&mut self) {
        self.room_state = RoomState::Invited;
    }

    /// Set the membership RoomState of this Room
    pub fn set_state(&mut self, room_state: RoomState) {
        self.room_state = room_state;
    }

    /// Mark this Room as having all the members synced.
    pub fn mark_members_synced(&mut self) {
        self.members_synced = true;
    }

    /// Mark this Room as still missing member information.
    pub fn mark_members_missing(&mut self) {
        self.members_synced = false;
    }

    /// Mark this Room as still missing some state information.
    pub fn mark_state_partially_synced(&mut self) {
        self.sync_info = SyncInfo::PartiallySynced;
    }

    /// Mark this Room as still having all state synced.
    pub fn mark_state_fully_synced(&mut self) {
        self.sync_info = SyncInfo::FullySynced;
    }

    /// Mark this Room as still having no state synced.
    pub fn mark_state_not_synced(&mut self) {
        self.sync_info = SyncInfo::NoState;
    }

    /// Mark this Room as having the encryption state synced.
    pub fn mark_encryption_state_synced(&mut self) {
        self.encryption_state_synced = true;
    }

    /// Mark this Room as still missing encryption state information.
    pub fn mark_encryption_state_missing(&mut self) {
        self.encryption_state_synced = false;
    }

    /// Set the `prev_batch`-token.
    /// Returns whether the token has differed and thus has been upgraded:
    /// `false` means no update was applied as the were the same
    pub fn set_prev_batch(&mut self, prev_batch: Option<&str>) -> bool {
        if self.last_prev_batch.as_deref() != prev_batch {
            self.last_prev_batch = prev_batch.map(|p| p.to_owned());
            true
        } else {
            false
        }
    }

    /// Returns the state this room is in.
    pub fn state(&self) -> RoomState {
        self.room_state
    }

    /// Returns whether this is an encrypted room.
    pub fn is_encrypted(&self) -> bool {
        self.base_info.encryption.is_some()
    }

    /// Set the encryption event content in this room.
    pub fn set_encryption_event(&mut self, event: Option<RoomEncryptionEventContent>) {
        self.base_info.encryption = event;
    }

    /// Handle the given state event.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_state_event(&mut self, event: &AnySyncStateEvent) -> bool {
        self.base_info.handle_state_event(event)
    }

    /// Handle the given stripped state event.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_stripped_state_event(&mut self, event: &AnyStrippedStateEvent) -> bool {
        self.base_info.handle_stripped_state_event(event)
    }

    /// Handle the given redaction.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_redaction(&mut self, event: &OriginalSyncRoomRedactionEvent) {
        self.base_info.handle_redaction(event);
    }

    /// Update the room name
    pub fn update_name(&mut self, name: String) {
        self.base_info.name = Some(MinimalStateEvent::Original(OriginalMinimalStateEvent {
            content: RoomNameEventContent::new(Some(name)),
            event_id: None,
        }));
    }

    /// Update the notifications count
    pub fn update_notification_count(&mut self, notification_counts: UnreadNotificationsCount) {
        self.notification_counts = notification_counts;
    }

    /// Update the RoomSummary
    ///
    /// Returns true if the Summary modified the info, false otherwise.
    pub fn update_summary(&mut self, summary: &RumaSummary) -> bool {
        let mut changed = false;

        if !summary.is_empty() {
            if !summary.heroes.is_empty() {
                self.summary.heroes = summary.heroes.clone();
                changed = true;
            }

            if let Some(joined) = summary.joined_member_count {
                self.summary.joined_member_count = joined.into();
                changed = true;
            }

            if let Some(invited) = summary.invited_member_count {
                self.summary.invited_member_count = invited.into();
                changed = true;
            }
        }

        changed
    }

    /// The number of active members (invited + joined) in the room.
    ///
    /// The return value is saturated at `u64::MAX`.
    pub fn active_members_count(&self) -> u64 {
        self.summary.joined_member_count.saturating_add(self.summary.invited_member_count)
    }

    /// The number of invited members in the room
    pub fn invited_members_count(&self) -> u64 {
        self.summary.invited_member_count
    }

    /// The number of joined members in the room
    pub fn joined_members_count(&self) -> u64 {
        self.summary.joined_member_count
    }

    /// Get the canonical alias of this room.
    pub fn canonical_alias(&self) -> Option<&RoomAliasId> {
        self.base_info.canonical_alias.as_ref()?.as_original()?.content.alias.as_deref()
    }

    /// Get the alternative aliases of this room.
    pub fn alt_aliases(&self) -> &[OwnedRoomAliasId] {
        self.base_info
            .canonical_alias
            .as_ref()
            .and_then(|ev| ev.as_original())
            .map(|ev| ev.content.alt_aliases.as_ref())
            .unwrap_or_default()
    }

    /// Get the room ID of this room.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Get the room version of this room.
    pub fn room_version(&self) -> Option<&RoomVersionId> {
        Some(&self.base_info.create.as_ref()?.as_original()?.content.room_version)
    }

    /// Get the room type of this room.
    pub fn room_type(&self) -> Option<&RoomType> {
        self.base_info.create.as_ref()?.as_original()?.content.room_type.as_ref()
    }

    fn creator(&self) -> Option<&UserId> {
        Some(match self.base_info.create.as_ref()? {
            MinimalStateEvent::Original(ev) => &ev.content.creator,
            MinimalStateEvent::Redacted(ev) => &ev.content.creator,
        })
    }

    fn guest_access(&self) -> &GuestAccess {
        match &self.base_info.guest_access {
            Some(MinimalStateEvent::Original(ev)) => &ev.content.guest_access,
            _ => &GuestAccess::Forbidden,
        }
    }

    fn history_visibility(&self) -> &HistoryVisibility {
        match &self.base_info.history_visibility {
            Some(MinimalStateEvent::Original(ev)) => &ev.content.history_visibility,
            _ => &HistoryVisibility::WorldReadable,
        }
    }

    fn join_rule(&self) -> &JoinRule {
        match &self.base_info.join_rules {
            Some(MinimalStateEvent::Original(ev)) => &ev.content.join_rule,
            _ => &JoinRule::Public,
        }
    }

    fn name(&self) -> Option<&str> {
        Some(self.base_info.name.as_ref()?.as_original()?.content.name.as_ref()?.as_ref())
    }

    fn tombstone(&self) -> Option<&RoomTombstoneEventContent> {
        Some(&self.base_info.tombstone.as_ref()?.as_original()?.content)
    }

    fn topic(&self) -> Option<&str> {
        Some(&self.base_info.topic.as_ref()?.as_original()?.content.topic)
    }
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

        states
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use assign::assign;
    use matrix_sdk_test::async_test;
    use ruma::{
        events::{
            room::{
                canonical_alias::RoomCanonicalAliasEventContent,
                member::{
                    MembershipState, RoomMemberEventContent, StrippedRoomMemberEvent,
                    SyncRoomMemberEvent,
                },
                name::RoomNameEventContent,
            },
            StateEventType,
        },
        room_alias_id, room_id,
        serde::Raw,
        user_id,
    };
    use serde_json::json;

    use super::*;
    use crate::{
        store::{MemoryStore, StateChanges, StateStore},
        MinimalStateEvent, OriginalMinimalStateEvent,
    };

    #[test]
    #[cfg(feature = "experimental-sliding-sync")]
    fn room_info_serialization() {
        // This test exists to make sure we don't accidentally change the
        // serialized format for `RoomInfo`.

        let info = RoomInfo {
            room_id: room_id!("!gda78o:server.tld").into(),
            room_state: RoomState::Invited,
            notification_counts: UnreadNotificationsCount {
                highlight_count: 1,
                notification_count: 2,
            },
            summary: RoomSummary {
                heroes: vec!["Somebody".to_owned()],
                joined_member_count: 5,
                invited_member_count: 0,
            },
            members_synced: true,
            last_prev_batch: Some("pb".to_owned()),
            sync_info: SyncInfo::FullySynced,
            encryption_state_synced: true,
            latest_event: Some(
                Raw::from_json_string(json!({"sender": "@u:i.uk"}).to_string()).unwrap().into(),
            ),
            base_info: BaseRoomInfo::new(),
        };

        let info_json = json!({
            "room_id": "!gda78o:server.tld",
            "room_type": "Invited",
            "notification_counts": {
                "highlight_count": 1,
                "notification_count": 2,
            },
            "summary": {
                "heroes": ["Somebody"],
                "joined_member_count": 5,
                "invited_member_count": 0,
            },
            "members_synced": true,
            "last_prev_batch": "pb",
            "sync_info": "FullySynced",
            "encryption_state_synced": true,
            "latest_event": {"encryption_info": null, "event": {"sender": "@u:i.uk"}},
            "base_info": {
                "avatar": null,
                "canonical_alias": null,
                "create": null,
                "dm_targets": [],
                "encryption": null,
                "guest_access": null,
                "history_visibility": null,
                "join_rules": null,
                "max_power_level": 100,
                "name": null,
                "tombstone": null,
                "topic": null,
            }
        });

        assert_eq!(serde_json::to_value(info).unwrap(), info_json);
    }

    #[test]
    #[cfg(feature = "experimental-sliding-sync")]
    fn room_info_deserialization_without_optional_items() {
        // Ensure we can still deserialize RoomInfos before we added things to its
        // schema

        // The following JSON should never change if we want to be able to read in old
        // cached state
        let info_json = json!({
            "room_id": "!gda78o:server.tld",
            "room_type": "Invited",
            "notification_counts": {
                "highlight_count": 1,
                "notification_count": 2,
            },
            "summary": {
                "heroes": ["Somebody"],
                "joined_member_count": 5,
                "invited_member_count": 0,
            },
            "members_synced": true,
            "last_prev_batch": "pb",
            "sync_info": "FullySynced",
            "encryption_state_synced": true,
            "base_info": {
                "avatar": null,
                "canonical_alias": null,
                "create": null,
                "dm_targets": [],
                "encryption": null,
                "guest_access": null,
                "history_visibility": null,
                "join_rules": null,
                "max_power_level": 100,
                "name": null,
                "tombstone": null,
                "topic": null,
            }
        });

        let info: RoomInfo = serde_json::from_value(info_json).unwrap();

        assert_eq!(info.room_id, room_id!("!gda78o:server.tld"));
        assert_eq!(info.room_state, RoomState::Invited);
        assert_eq!(info.notification_counts.highlight_count, 1);
        assert_eq!(info.notification_counts.notification_count, 2);
        assert_eq!(info.summary.heroes, vec!["Somebody".to_owned()]);
        assert_eq!(info.summary.joined_member_count, 5);
        assert_eq!(info.summary.invited_member_count, 0);
        assert!(info.members_synced);
        assert_eq!(info.last_prev_batch, Some("pb".to_owned()));
        assert_eq!(info.sync_info, SyncInfo::FullySynced);
        assert!(info.encryption_state_synced);
        assert!(info.latest_event.is_none());
        assert!(info.base_info.avatar.is_none());
        assert!(info.base_info.canonical_alias.is_none());
        assert!(info.base_info.create.is_none());
        assert_eq!(info.base_info.dm_targets.len(), 0);
        assert!(info.base_info.encryption.is_none());
        assert!(info.base_info.guest_access.is_none());
        assert!(info.base_info.history_visibility.is_none());
        assert!(info.base_info.join_rules.is_none());
        assert_eq!(info.base_info.max_power_level, 100);
        assert!(info.base_info.name.is_none());
        assert!(info.base_info.tombstone.is_none());
        assert!(info.base_info.topic.is_none());
    }

    fn make_room(room_type: RoomState) -> (Arc<MemoryStore>, Room) {
        let store = Arc::new(MemoryStore::new());
        let user_id = user_id!("@me:example.org");
        let room_id = room_id!("!test:localhost");

        (store.clone(), Room::new(user_id, store, room_id, room_type))
    }

    fn make_stripped_member_event(user_id: &UserId, name: &str) -> Raw<StrippedRoomMemberEvent> {
        let ev_json = json!({
            "type": "m.room.member",
            "content": assign!(RoomMemberEventContent::new(MembershipState::Join), {
                displayname: Some(name.to_owned())
            }),
            "sender": user_id,
            "state_key": user_id,
        });

        Raw::new(&ev_json).unwrap().cast()
    }

    fn make_member_event(user_id: &UserId, name: &str) -> Raw<SyncRoomMemberEvent> {
        let ev_json = json!({
            "type": "m.room.member",
            "content": assign!(RoomMemberEventContent::new(MembershipState::Join), {
                displayname: Some(name.to_owned())
            }),
            "sender": user_id,
            "state_key": user_id,
            "event_id": "$h29iv0s1:example.com",
            "origin_server_ts": 208,
        });

        Raw::new(&ev_json).unwrap().cast()
    }

    #[async_test]
    async fn display_name_for_joined_room_is_empty_if_no_info() {
        let (_, room) = make_room(RoomState::Joined);
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Empty);
    }

    #[async_test]
    async fn display_name_for_joined_room_uses_canonical_alias_if_available() {
        let (_, room) = make_room(RoomState::Joined);
        room.inner.write().unwrap().base_info.canonical_alias = Some(make_canonical_alias_event());
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Aliased("test".to_owned()));
    }

    #[async_test]
    async fn display_name_for_joined_room_prefers_name_over_alias() {
        let (_, room) = make_room(RoomState::Joined);
        room.inner.write().unwrap().base_info.canonical_alias = Some(make_canonical_alias_event());
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Aliased("test".to_owned()));
        room.inner.write().unwrap().base_info.name = Some(make_name_event());
        // Display name wasn't cached when we asked for it above, and name overrides
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Named("Test Room".to_owned()));
    }

    #[async_test]
    async fn display_name_for_invited_room_is_empty_if_no_info() {
        let (_, room) = make_room(RoomState::Invited);
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Empty);
    }

    #[async_test]
    async fn display_name_for_invited_room_uses_canonical_alias_if_available() {
        let (_, room) = make_room(RoomState::Invited);
        room.inner.write().unwrap().base_info.canonical_alias = Some(make_canonical_alias_event());
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Aliased("test".to_owned()));
    }

    #[async_test]
    async fn display_name_for_invited_room_prefers_name_over_alias() {
        let (_, room) = make_room(RoomState::Invited);
        room.inner.write().unwrap().base_info.canonical_alias = Some(make_canonical_alias_event());
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Aliased("test".to_owned()));
        room.inner.write().unwrap().base_info.name = Some(make_name_event());
        // Display name wasn't cached when we asked for it above, and name overrides
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Named("Test Room".to_owned()));
    }

    fn make_canonical_alias_event() -> MinimalStateEvent<RoomCanonicalAliasEventContent> {
        MinimalStateEvent::Original(OriginalMinimalStateEvent {
            content: assign!(RoomCanonicalAliasEventContent::new(), {
                alias: Some(room_alias_id!("#test:example.com").to_owned()),
            }),
            event_id: None,
        })
    }

    fn make_name_event() -> MinimalStateEvent<RoomNameEventContent> {
        MinimalStateEvent::Original(OriginalMinimalStateEvent {
            content: RoomNameEventContent::new(Some("Test Room".try_into().unwrap())),
            event_id: None,
        })
    }

    #[async_test]
    async fn test_display_name_dm_invited() {
        let (store, room) = make_room(RoomState::Invited);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            heroes: vec![me.to_string(), matthew.to_string()],
        });

        changes.add_stripped_member(
            room_id,
            matthew,
            make_stripped_member_event(matthew, "Matthew"),
        );
        changes.add_stripped_member(room_id, me, make_stripped_member_event(me, "Me"));
        store.save_changes(&changes).await.unwrap();

        room.inner.write().unwrap().update_summary(&summary);
        assert_eq!(
            room.display_name().await.unwrap(),
            DisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_invited_no_heroes() {
        let (store, room) = make_room(RoomState::Invited);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());

        changes.add_stripped_member(
            room_id,
            matthew,
            make_stripped_member_event(matthew, "Matthew"),
        );
        changes.add_stripped_member(room_id, me, make_stripped_member_event(me, "Me"));
        store.save_changes(&changes).await.unwrap();

        assert_eq!(
            room.display_name().await.unwrap(),
            DisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_joined() {
        let (store, room) = make_room(RoomState::Joined);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            joined_member_count: Some(2u32.into()),
            heroes: vec![me.to_string(), matthew.to_string()],
        });

        let members = changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        members.insert(matthew.into(), make_member_event(matthew, "Matthew").cast());
        members.insert(me.into(), make_member_event(me, "Me").cast());

        store.save_changes(&changes).await.unwrap();

        room.inner.write().unwrap().update_summary(&summary);
        assert_eq!(
            room.display_name().await.unwrap(),
            DisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_joined_no_heroes() {
        let (store, room) = make_room(RoomState::Joined);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());

        let members = changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        members.insert(matthew.into(), make_member_event(matthew, "Matthew").cast());
        members.insert(me.into(), make_member_event(me, "Me").cast());

        store.save_changes(&changes).await.unwrap();

        assert_eq!(
            room.display_name().await.unwrap(),
            DisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_alone() {
        let (store, room) = make_room(RoomState::Joined);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            joined_member_count: Some(1u32.into()),
            heroes: vec![me.to_string(), matthew.to_string()],
        });

        let members = changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        members.insert(matthew.into(), make_member_event(matthew, "Matthew").cast());
        members.insert(me.into(), make_member_event(me, "Me").cast());

        store.save_changes(&changes).await.unwrap();

        room.inner.write().unwrap().update_summary(&summary);
        assert_eq!(room.display_name().await.unwrap(), DisplayName::EmptyWas("Matthew".to_owned()));
    }

    #[test]
    fn setting_the_name_on_room_info_creates_a_fake_event() {
        // Given a room
        let mut room_info = RoomInfo::new(room_id!("!r:e.uk"), RoomState::Joined);

        // When I update its name
        room_info.update_name("new name".to_owned());

        // Then it reports the name I provided
        assert_eq!(room_info.name(), Some("new name"));

        // And that is implemented by making a fake event
        assert_eq!(
            room_info
                .base_info
                .name
                .as_ref()
                .unwrap()
                .as_original()
                .unwrap()
                .content
                .name
                .as_ref()
                .unwrap(),
            "new name"
        );
    }

    #[test]
    #[cfg(feature = "experimental-sliding-sync")]
    fn when_we_provide_a_newly_decrypted_event_it_replaces_latest_event() {
        // Given a room with an encrypted event
        let (_store, mut room) = make_room(RoomState::Joined);
        add_encrypted_event(&mut room, "$A");
        // Sanity: it has no latest_event
        assert!(room.latest_event().is_none());

        // When I provide a decrypted event to replace the encrypted one
        let event = make_event("$A");
        room.on_latest_event_decrypted(event.clone(), 0);

        // Then is it stored
        assert_eq!(room.latest_event().unwrap().event_id(), event.event_id());
    }

    #[test]
    #[cfg(feature = "experimental-sliding-sync")]
    fn when_a_newly_decrypted_event_appears_we_delete_all_older_encrypted_events() {
        // Given a room with some encrypted events and a latest event
        let (_store, mut room) = make_room(RoomState::Joined);
        room.inner.write().unwrap().latest_event = Some(make_event("$A"));
        add_encrypted_event(&mut room, "$0");
        add_encrypted_event(&mut room, "$1");
        add_encrypted_event(&mut room, "$2");
        add_encrypted_event(&mut room, "$3");

        // When I provide a latest event
        let new_event = make_event("$1");
        let new_event_index = 1;
        room.on_latest_event_decrypted(new_event.clone(), new_event_index);

        // Then the encrypted events list is shortened to only newer events
        let enc_evs = room.latest_encrypted_events();
        assert_eq!(enc_evs.len(), 2);
        assert_eq!(enc_evs.get(0).unwrap().get_field::<&str>("event_id").unwrap().unwrap(), "$2");
        assert_eq!(enc_evs.get(1).unwrap().get_field::<&str>("event_id").unwrap().unwrap(), "$3");

        // And the event is stored
        assert_eq!(room.latest_event().unwrap().event_id(), new_event.event_id());
    }

    #[test]
    #[cfg(feature = "experimental-sliding-sync")]
    fn replacing_the_newest_event_leaves_none_left() {
        // Given a room with some encrypted events
        let (_store, mut room) = make_room(RoomState::Joined);
        add_encrypted_event(&mut room, "$0");
        add_encrypted_event(&mut room, "$1");
        add_encrypted_event(&mut room, "$2");
        add_encrypted_event(&mut room, "$3");

        // When I provide a latest event and say it was the very latest
        let new_event = make_event("$3");
        let new_event_index = 3;
        room.on_latest_event_decrypted(new_event.clone(), new_event_index);

        // Then the encrypted events list ie empty
        let enc_evs = room.latest_encrypted_events();
        assert_eq!(enc_evs.len(), 0);
    }

    #[cfg(feature = "experimental-sliding-sync")]
    fn add_encrypted_event(room: &mut Room, event_id: &str) {
        room.latest_encrypted_events
            .write()
            .unwrap()
            .push(Raw::from_json_string(json!({ "event_id": event_id }).to_string()).unwrap());
    }

    #[cfg(feature = "experimental-sliding-sync")]
    fn make_event(event_id: &str) -> SyncTimelineEvent {
        SyncTimelineEvent::new(
            Raw::from_json_string(json!({ "event_id": event_id }).to_string()).unwrap(),
        )
    }
}
