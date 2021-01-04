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
    stream::{self, Stream, StreamExt},
};
use matrix_sdk_common::{
    api::r0::sync::sync_events::RoomSummary as RumaSummary,
    events::{room::tombstone::TombstoneEventContent, AnySyncStateEvent, EventType},
    identifiers::{RoomAliasId, RoomId, UserId},
};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{responses::UnreadNotificationsCount, store::SledStore};

use super::{BaseRoomInfo, RoomMember};

#[derive(Debug, Clone)]
pub struct Room {
    room_id: Arc<RoomId>,
    own_user_id: Arc<UserId>,
    inner: Arc<SyncRwLock<RoomInfo>>,
    store: SledStore,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoomSummary {
    heroes: Vec<String>,
    joined_member_count: u64,
    invited_member_count: u64,
}

/// Signals to the `BaseClient` which `RoomState` to send to `EventEmitter`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum RoomType {
    /// Represents a joined room, the `joined_rooms` HashMap will be used.
    Joined,
    /// Represents a left room, the `left_rooms` HashMap will be used.
    Left,
    /// Represents an invited room, the `invited_rooms` HashMap will be used.
    Invited,
}

impl Room {
    pub fn new(
        own_user_id: &UserId,
        store: SledStore,
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

    pub fn restore(own_user_id: &UserId, store: SledStore, room_info: RoomInfo) -> Self {
        Self {
            own_user_id: Arc::new(own_user_id.clone()),
            room_id: room_info.room_id.clone(),
            store,
            inner: Arc::new(SyncRwLock::new(room_info)),
        }
    }

    pub fn are_members_synced(&self) -> bool {
        self.inner.read().unwrap().members_synced
    }

    pub fn room_type(&self) -> RoomType {
        self.inner.read().unwrap().room_type
    }

    pub fn is_direct(&self) -> bool {
        self.inner.read().unwrap().base_info.dm_target.is_some()
    }

    pub fn direct_target(&self) -> Option<UserId> {
        self.inner.read().unwrap().base_info.dm_target.clone()
    }

    fn max_power_level(&self) -> i64 {
        self.inner.read().unwrap().base_info.max_power_level
    }

    pub async fn get_joined_user_ids(&self) -> impl Stream<Item = UserId> {
        self.store.get_joined_user_ids(self.room_id()).await
    }

    pub async fn get_joined_members(&self) -> impl Stream<Item = RoomMember> + '_ {
        let joined = self.store.get_joined_user_ids(self.room_id()).await;

        joined.filter_map(move |u| async move { self.get_member(&u).await })
    }

    pub async fn get_active_members(&self) -> impl Stream<Item = RoomMember> + '_ {
        let joined = self.store.get_joined_user_ids(self.room_id()).await;
        let invited = self.store.get_invited_user_ids(self.room_id()).await;

        joined
            .chain(invited)
            .filter_map(move |u| async move { self.get_member(&u).await })
    }

    /// Calculate the canonical display name of the room, taking into account
    /// its name, aliases and members.
    ///
    /// The display name is calculated according to [this algorithm][spec].
    ///
    /// [spec]:
    /// <https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room>
    #[allow(clippy::await_holding_lock)]
    async fn calculate_name(&self) -> String {
        let inner = self.inner.read().unwrap();

        if let Some(name) = &inner.base_info.name {
            let name = name.trim();
            name.to_string()
        } else if let Some(alias) = &inner.base_info.canonical_alias {
            let alias = alias.alias().trim();
            alias.to_string()
        } else {
            let joined = inner.summary.joined_member_count;
            let invited = inner.summary.invited_member_count;
            let heroes_count = inner.summary.heroes.len() as u64;
            let invited_joined = (invited + joined).saturating_sub(1);

            info!(
                "Calculating name for {}, own user {} hero count {} heroes {:#?}",
                self.room_id(),
                self.own_user_id,
                heroes_count,
                inner.summary.heroes
            );
            let own_user_id = self.own_user_id.clone();

            let is_own_member = |m: &RoomMember| m.user_id() == &*own_user_id;

            if !inner.summary.heroes.is_empty() {
                let mut names = stream::iter(inner.summary.heroes.iter())
                    .take(3)
                    .filter_map(|u| async move {
                        let user_id = UserId::try_from(u.as_str()).ok()?;
                        self.get_member(&user_id).await
                    })
                    .map(|mem| {
                        mem.display_name()
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| mem.user_id().localpart().to_string())
                    })
                    .collect::<Vec<String>>()
                    .await;
                names.sort();
                names.join(", ")
            } else if heroes_count >= invited_joined {
                let members = self.get_active_members().await;

                let mut names = members
                    .filter(|m| future::ready(is_own_member(m)))
                    .take(3)
                    .map(|mem| {
                        mem.display_name()
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| mem.user_id().localpart().to_string())
                    })
                    .collect::<Vec<String>>()
                    .await;
                // stabilize ordering
                names.sort();
                names.join(", ")
            } else if heroes_count < invited_joined && invited + joined > 1 {
                let members = self.get_active_members().await;

                let mut names = members
                    .filter(|m| future::ready(is_own_member(m)))
                    .take(3)
                    .map(|mem| {
                        mem.display_name()
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| mem.user_id().localpart().to_string())
                    })
                    .collect::<Vec<String>>()
                    .await;
                names.sort();

                // TODO: What length does the spec want us to use here and in
                // the `else`?
                format!("{}, and {} others", names.join(", "), (joined + invited))
            } else {
                "Empty room".to_string()
            }
        }
    }

    pub fn own_user_id(&self) -> &UserId {
        &self.own_user_id
    }

    pub(crate) fn clone_info(&self) -> RoomInfo {
        (*self.inner.read().unwrap()).clone()
    }

    pub async fn joined_user_ids(&self) -> impl Stream<Item = UserId> {
        self.store.get_joined_user_ids(&self.room_id).await
    }

    pub fn is_encrypted(&self) -> bool {
        self.inner.read().unwrap().is_encrypted()
    }

    pub fn unread_notification_counts(&self) -> UnreadNotificationsCount {
        self.inner.read().unwrap().notification_counts
    }

    pub fn is_tombstoned(&self) -> bool {
        self.inner.read().unwrap().base_info.tombstone.is_some()
    }

    pub fn tombstone(&self) -> Option<TombstoneEventContent> {
        self.inner.read().unwrap().base_info.tombstone.clone()
    }

    pub fn topic(&self) -> Option<String> {
        self.inner.read().unwrap().base_info.topic.clone()
    }

    pub fn canonical_alias(&self) -> Option<RoomAliasId> {
        self.inner.read().unwrap().base_info.canonical_alias.clone()
    }

    pub fn update_summary(&self, summary: RoomInfo) {
        let mut inner = self.inner.write().unwrap();
        *inner = summary;
    }

    pub async fn get_member(&self, user_id: &UserId) -> Option<RoomMember> {
        let presence = self.store.get_presence_event(user_id).await;
        let profile = self.store.get_profile(self.room_id(), user_id).await;
        let max_power_level = self.max_power_level();

        let power = self
            .store
            .get_state_event(self.room_id(), EventType::RoomPowerLevels, "")
            .await
            .map(|e| {
                if let AnySyncStateEvent::RoomPowerLevels(e) = e {
                    Some(e)
                } else {
                    None
                }
            })
            .flatten();

        self.store
            .get_member_event(&self.room_id, user_id)
            .await
            .map(|e| RoomMember {
                event: e.into(),
                profile: profile.into(),
                presence: presence.into(),
                power_levles: power.into(),
                max_power_level,
            })
    }

    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    pub fn last_prev_batch(&self) -> Option<String> {
        self.inner.read().unwrap().last_prev_batch.clone()
    }

    pub async fn display_name(&self) -> String {
        self.calculate_name().await
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomInfo {
    pub room_id: Arc<RoomId>,
    pub room_type: RoomType,

    pub notification_counts: UnreadNotificationsCount,
    pub summary: RoomSummary,
    pub members_synced: bool,
    pub last_prev_batch: Option<String>,

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

    pub(crate) fn handle_state_event(&mut self, event: &AnySyncStateEvent) -> bool {
        self.base_info.handle_state_event(&event.content())
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
}
