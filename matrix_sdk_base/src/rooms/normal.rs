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
    convert::TryFrom,
    sync::{Arc, RwLock as SyncRwLock},
};

use futures::{
    future,
    stream::{self, StreamExt},
};
use matrix_sdk_common::{
    api::r0::sync::sync_events::RoomSummary as RumaSummary,
    events::{
        room::{
            create::CreateEventContent, encryption::EncryptionEventContent,
            guest_access::GuestAccess, history_visibility::HistoryVisibility, join_rules::JoinRule,
            tombstone::TombstoneEventContent,
        },
        tag::Tags,
        AnyBasicEvent, AnyStateEventContent, AnySyncStateEvent, EventType,
    },
    identifiers::{MxcUri, RoomAliasId, RoomId, UserId},
};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{
    deserialized_responses::UnreadNotificationsCount,
    store::{Result as StoreResult, StateStore},
};

use super::{BaseRoomInfo, RoomMember};

/// The underlying room data structure collecting state for joined, left and invtied rooms.
#[derive(Debug, Clone)]
pub struct Room {
    room_id: Arc<RoomId>,
    own_user_id: Arc<UserId>,
    inner: Arc<SyncRwLock<RoomInfo>>,
    store: Arc<Box<dyn StateStore>>,
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
pub enum RoomType {
    /// The room is in a joined state.
    Joined,
    /// The room is in a left state.
    Left,
    /// The room is in a invited state.
    Invited,
}

impl Room {
    pub(crate) fn new(
        own_user_id: &UserId,
        store: Arc<Box<dyn StateStore>>,
        room_id: &RoomId,
        room_type: RoomType,
    ) -> Self {
        let room_id = Arc::new(room_id.clone());

        let room_info = RoomInfo {
            room_id,
            room_type,
            notification_counts: Default::default(),
            summary: Default::default(),
            members_synced: false,
            last_prev_batch: None,
            base_info: BaseRoomInfo::new(),
        };

        Self::restore(own_user_id, store, room_info)
    }

    pub(crate) fn restore(
        own_user_id: &UserId,
        store: Arc<Box<dyn StateStore>>,
        room_info: RoomInfo,
    ) -> Self {
        Self {
            own_user_id: Arc::new(own_user_id.clone()),
            room_id: room_info.room_id.clone(),
            store,
            inner: Arc::new(SyncRwLock::new(room_info)),
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

    /// Get the type of the room.
    pub fn room_type(&self) -> RoomType {
        self.inner.read().unwrap().room_type
    }

    /// Get the unread notification counts.
    pub fn unread_notification_counts(&self) -> UnreadNotificationsCount {
        self.inner.read().unwrap().notification_counts
    }

    /// Check if the room has it's members fully synced.
    ///
    /// Members might be missing if lazy member loading was enabled for the sync.
    ///
    /// Returns true if no members are missing, false otherwise.
    pub fn are_members_synced(&self) -> bool {
        self.inner.read().unwrap().members_synced
    }

    /// Get the `prev_batch` token that was received from the last sync. May be
    /// `None` if the last sync contained the full room history.
    pub fn last_prev_batch(&self) -> Option<String> {
        self.inner.read().unwrap().last_prev_batch.clone()
    }

    /// Get the avatar url of this room.
    pub fn avatar_url(&self) -> Option<MxcUri> {
        self.inner.read().unwrap().base_info.avatar_url.clone()
    }

    /// Get the canonical alias of this room.
    pub fn canonical_alias(&self) -> Option<RoomAliasId> {
        self.inner.read().unwrap().base_info.canonical_alias.clone()
    }

    /// Get the `m.room.create` content of this room.
    ///
    /// This usually isn't optional but some servers might not send an
    /// `m.room.create` event as the first event for a given room, thus this can
    /// be optional.
    pub fn create_content(&self) -> Option<CreateEventContent> {
        self.inner.read().unwrap().base_info.create.clone()
    }

    /// Is this room considered a direct message.
    pub fn is_direct(&self) -> bool {
        self.inner.read().unwrap().base_info.dm_target.is_some()
    }

    /// If this room is a direct message, get the member that we're sharing the
    /// room with.
    ///
    /// *Note*: The member list might have been moddified in the meantime and
    /// the target might not even be in the room anymore. This setting should
    /// only be considered as guidance.
    pub fn direct_target(&self) -> Option<UserId> {
        self.inner.read().unwrap().base_info.dm_target.clone()
    }

    /// Is the room encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.inner.read().unwrap().is_encrypted()
    }

    /// Get the `m.room.encryption` content that enabled end to end encryption
    /// in the room.
    pub fn encryption_settings(&self) -> Option<EncryptionEventContent> {
        self.inner.read().unwrap().base_info.encryption.clone()
    }

    /// Get the guest access policy of this room.
    pub fn guest_access(&self) -> GuestAccess {
        self.inner.read().unwrap().base_info.guest_access.clone()
    }

    /// Get the history visibility policy of this room.
    pub fn history_visibility(&self) -> HistoryVisibility {
        self.inner
            .read()
            .unwrap()
            .base_info
            .history_visibility
            .clone()
    }

    /// Is the room considered to be public.
    pub fn is_public(&self) -> bool {
        matches!(self.join_rule(), JoinRule::Public)
    }

    /// Get the join rule policy of this room.
    pub fn join_rule(&self) -> JoinRule {
        self.inner.read().unwrap().base_info.join_rule.clone()
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
        self.inner.read().unwrap().base_info.name.clone()
    }

    /// Has the room been tombstoned.
    pub fn is_tombstoned(&self) -> bool {
        self.inner.read().unwrap().base_info.tombstone.is_some()
    }

    /// Get the `m.room.tombstone` content of this room if there is one.
    pub fn tombstone(&self) -> Option<TombstoneEventContent> {
        self.inner.read().unwrap().base_info.tombstone.clone()
    }

    /// Get the topic of the room.
    pub fn topic(&self) -> Option<String> {
        self.inner.read().unwrap().base_info.topic.clone()
    }

    /// Calculate the canonical display name of the room, taking into account
    /// its name, aliases and members.
    ///
    /// The display name is calculated according to [this algorithm][spec].
    ///
    /// [spec]: <https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room>
    pub async fn display_name(&self) -> StoreResult<String> {
        self.calculate_name().await
    }

    /// Get the list of users ids that are considered to be joined members of
    /// this room.
    pub async fn joined_user_ids(&self) -> StoreResult<Vec<UserId>> {
        self.store.get_joined_user_ids(self.room_id()).await
    }

    /// Get the all `RoomMember`s of this room that are known to the store.
    pub async fn members(&self) -> StoreResult<Vec<RoomMember>> {
        let user_ids = self.store.get_user_ids(self.room_id()).await?;
        let mut members = Vec::new();

        for u in user_ids {
            let m = self.get_member(&u).await?;

            if let Some(member) = m {
                members.push(member);
            }
        }

        Ok(members)
    }

    /// Get the list of `RoomMember`s that are considered to be joined members
    /// of this room.
    pub async fn joined_members(&self) -> StoreResult<Vec<RoomMember>> {
        let joined = self.store.get_joined_user_ids(self.room_id()).await?;
        let mut members = Vec::new();

        for u in joined {
            let m = self.get_member(&u).await?;

            if let Some(member) = m {
                members.push(member);
            }
        }

        Ok(members)
    }

    /// Get the list of `RoomMember`s that are considered to be joined or
    /// invited members of this room.
    pub async fn active_members(&self) -> StoreResult<Vec<RoomMember>> {
        let joined = self.store.get_joined_user_ids(self.room_id()).await?;
        let invited = self.store.get_invited_user_ids(self.room_id()).await?;

        let mut members = Vec::new();

        for u in joined.iter().chain(&invited) {
            let m = self.get_member(u).await?;

            if let Some(member) = m {
                members.push(member);
            }
        }

        Ok(members)
    }

    async fn calculate_name(&self) -> StoreResult<String> {
        let summary = {
            let inner = self.inner.read().unwrap();

            if let Some(name) = &inner.base_info.name {
                let name = name.trim();
                return Ok(name.to_string());
            } else if let Some(alias) = &inner.base_info.canonical_alias {
                let alias = alias.alias().trim();
                return Ok(alias.to_string());
            }
            inner.summary.clone()
        };
        // TODO what should we do here? We have correct counts only if lazy
        // loading is used.
        let joined = summary.joined_member_count;
        let invited = summary.invited_member_count;
        let heroes_count = summary.heroes.len() as u64;

        let is_own_member = |m: &RoomMember| m.user_id() == &*self.own_user_id;
        let is_own_user_id = |u: &str| u == self.own_user_id().as_str();

        let members: Vec<RoomMember> = if summary.heroes.is_empty() {
            self.active_members()
                .await?
                .into_iter()
                .filter(|u| !is_own_member(&u))
                .take(5)
                .collect()
        } else {
            let members: Vec<_> = stream::iter(summary.heroes.iter())
                .filter(|u| future::ready(!is_own_user_id(u)))
                .filter_map(|u| async move {
                    let user_id = UserId::try_from(u.as_str()).ok()?;
                    self.get_member(&user_id).await.transpose()
                })
                .collect()
                .await;

            let members: StoreResult<Vec<_>> = members.into_iter().collect();

            members?
        };

        info!(
            "Calculating name for {}, own user {} hero count {} heroes {:#?}",
            self.room_id(),
            self.own_user_id,
            heroes_count,
            summary.heroes
        );

        let inner = self.inner.read().unwrap();
        Ok(inner
            .base_info
            .calculate_room_name(joined, invited, members))
    }

    pub(crate) fn clone_info(&self) -> RoomInfo {
        (*self.inner.read().unwrap()).clone()
    }

    pub(crate) fn update_summary(&self, summary: RoomInfo) {
        let mut inner = self.inner.write().unwrap();
        *inner = summary;
    }

    /// Get the `RoomMember` with the given `user_id`.
    ///
    /// Returns `None` if the member was never part of this room, otherwise
    /// return a `RoomMember` that can be in a joined, invited, left, banned
    /// state.
    pub async fn get_member(&self, user_id: &UserId) -> StoreResult<Option<RoomMember>> {
        let member_event =
            if let Some(m) = self.store.get_member_event(self.room_id(), user_id).await? {
                m
            } else {
                return Ok(None);
            };

        let presence = self
            .store
            .get_presence_event(user_id)
            .await?
            .and_then(|e| e.deserialize().ok());
        let profile = self.store.get_profile(self.room_id(), user_id).await?;
        let max_power_level = self.max_power_level();
        let is_room_creator = self
            .inner
            .read()
            .unwrap()
            .base_info
            .create
            .as_ref()
            .map(|c| &c.creator == user_id)
            .unwrap_or(false);

        let power = self
            .store
            .get_state_event(self.room_id(), EventType::RoomPowerLevels, "")
            .await?
            .and_then(|e| e.deserialize().ok())
            .and_then(|e| {
                if let AnySyncStateEvent::RoomPowerLevels(e) = e {
                    Some(e)
                } else {
                    None
                }
            });

        let ambiguous = self
            .store
            .get_users_with_display_name(
                self.room_id(),
                member_event
                    .content
                    .displayname
                    .as_deref()
                    .unwrap_or_else(|| user_id.localpart()),
            )
            .await?
            .len()
            > 1;

        Ok(Some(RoomMember {
            event: member_event.into(),
            profile: profile.into(),
            presence: presence.into(),
            power_levels: power.into(),
            max_power_level,
            is_room_creator,
            display_name_ambiguous: ambiguous,
        }))
    }

    /// Get the `Tags` for this room.
    pub async fn tags(&self) -> StoreResult<Option<Tags>> {
        if let Some(AnyBasicEvent::Tag(event)) = self
            .store
            .get_room_account_data_event(self.room_id(), EventType::Tag)
            .await?
            .and_then(|r| r.deserialize().ok())
        {
            Ok(Some(event.content.tags))
        } else {
            Ok(None)
        }
    }
}

/// The underlying pure data structure for joined and left rooms.
///
/// Holds all the info needed to persist a room into the state store.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomInfo {
    /// The unique room id of the room.
    pub room_id: Arc<RoomId>,
    /// The type of the room.
    pub room_type: RoomType,
    /// The unread notifications counts.
    pub notification_counts: UnreadNotificationsCount,
    /// The summary of this room.
    pub summary: RoomSummary,
    /// Flag remembering if the room members are synced.
    pub members_synced: bool,
    /// The prev batch of this room we received durring the last sync.
    pub last_prev_batch: Option<String>,
    /// Base room info which holds some basic event contents important for the
    /// room state.
    pub base_info: BaseRoomInfo,
}

impl RoomInfo {
    pub(crate) fn mark_as_joined(&mut self) {
        self.room_type = RoomType::Joined;
    }

    pub(crate) fn mark_as_left(&mut self) {
        self.room_type = RoomType::Left;
    }

    pub(crate) fn mark_as_invited(&mut self) {
        self.room_type = RoomType::Invited;
    }

    pub(crate) fn mark_members_synced(&mut self) {
        self.members_synced = true;
    }

    pub(crate) fn mark_members_missing(&mut self) {
        self.members_synced = false;
    }

    pub(crate) fn set_prev_batch(&mut self, prev_batch: Option<&str>) -> bool {
        if self.last_prev_batch.as_deref() != prev_batch {
            self.last_prev_batch = prev_batch.map(|p| p.to_string());
            true
        } else {
            false
        }
    }

    pub(crate) fn is_encrypted(&self) -> bool {
        self.base_info.encryption.is_some()
    }

    pub(crate) fn handle_state_event(&mut self, event: &AnyStateEventContent) -> bool {
        self.base_info.handle_state_event(&event)
    }

    pub(crate) fn update_notification_count(
        &mut self,
        notification_counts: UnreadNotificationsCount,
    ) {
        self.notification_counts = notification_counts;
    }

    pub(crate) fn update_summary(&mut self, summary: &RumaSummary) -> bool {
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
        self.summary
            .joined_member_count
            .saturating_add(self.summary.invited_member_count)
    }
}
