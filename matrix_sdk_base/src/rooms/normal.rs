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
    sync::{Arc, Mutex as SyncMutex},
};

use futures::{
    future,
    stream::{self, Stream, StreamExt},
};
use matrix_sdk_common::{
    api::r0::sync::sync_events::RoomSummary as RumaSummary,
    events::{room::encryption::EncryptionEventContent, AnySyncStateEvent, EventType},
    identifiers::{RoomAliasId, RoomId, UserId},
};
use serde::{Deserialize, Serialize};

use crate::{responses::UnreadNotificationsCount, store::Store};

use super::RoomMember;

#[derive(Debug, Clone)]
pub struct Room {
    room_id: Arc<RoomId>,
    own_user_id: Arc<UserId>,
    inner: Arc<SyncMutex<RoomInfo>>,
    store: Store,
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
    pub fn new(own_user_id: &UserId, store: Store, room_id: &RoomId, room_type: RoomType) -> Self {
        let room_id = Arc::new(room_id.clone());

        Self {
            own_user_id: Arc::new(own_user_id.clone()),
            room_id: room_id.clone(),
            store,
            inner: Arc::new(SyncMutex::new(RoomInfo {
                room_id,
                room_type,
                encryption: None,
                summary: Default::default(),
                last_prev_batch: None,
                members_synced: false,
                name: None,
                canonical_alias: None,
                avatar_url: None,
                topic: None,
                notification_counts: Default::default(),
            })),
        }
    }

    pub fn are_members_synced(&self) -> bool {
        self.inner.lock().unwrap().members_synced
    }

    pub async fn get_j_members(&self) -> impl Stream<Item = RoomMember> + '_ {
        let joined = self.store.get_joined_user_ids(self.room_id()).await;
        let invited = self.store.get_invited_user_ids(self.room_id()).await;

        let x = move |u| async move {
            let presence = self.store.get_presence_event(&u).await;
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
                .get_member_event(self.room_id(), &u)
                .await
                .map(|m| RoomMember {
                    event: m.into(),
                    presence: presence.into(),
                    power_levles: power.into(),
                })
        };

        joined.chain(invited).filter_map(x)
    }

    /// Calculate the canonical display name of the room, taking into account
    /// its name, aliases and members.
    ///
    /// The display name is calculated according to [this algorithm][spec].
    ///
    /// [spec]:
    /// <https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room>
    pub async fn calculate_name(&self) -> String {
        let inner = self.inner.lock().unwrap();

        if let Some(name) = &inner.name {
            let name = name.trim();
            name.to_string()
        } else if let Some(alias) = &inner.canonical_alias {
            let alias = alias.alias().trim();
            alias.to_string()
        } else {
            let joined = inner.summary.joined_member_count;
            let invited = inner.summary.invited_member_count;
            let heroes_count = inner.summary.heroes.len() as u64;
            let invited_joined = (invited + joined).saturating_sub(1);

            let members = self.get_j_members().await;

            // info!(
            //     "Calculating name for {}, hero count {} members {:#?}",
            //     self.room_id(),
            //     heroes_count,
            //     members
            // );
            // TODO: This should use `self.heroes` but it is always empty??
            //
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

    pub(crate) fn clone_summary(&self) -> RoomInfo {
        (*self.inner.lock().unwrap()).clone()
    }

    pub async fn joined_user_ids(&self) -> impl Stream<Item = UserId> {
        self.store.get_joined_user_ids(&self.room_id).await
    }

    pub fn is_encrypted(&self) -> bool {
        self.inner.lock().unwrap().encryption.is_some()
    }

    pub fn update_summary(&self, summary: RoomInfo) {
        let mut inner = self.inner.lock().unwrap();
        *inner = summary;
    }

    pub async fn get_member(&self, user_id: &UserId) -> Option<RoomMember> {
        let presence = self.store.get_presence_event(user_id).await;
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
                presence: presence.into(),
                power_levles: power.into(),
            })
    }

    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    pub fn last_prev_batch(&self) -> Option<String> {
        self.inner.lock().unwrap().last_prev_batch.clone()
    }

    pub async fn display_name(&self) -> String {
        self.calculate_name().await
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomInfo {
    pub room_id: Arc<RoomId>,
    pub room_type: RoomType,

    pub name: Option<String>,
    pub canonical_alias: Option<RoomAliasId>,
    pub avatar_url: Option<String>,
    pub topic: Option<String>,

    pub notification_counts: UnreadNotificationsCount,
    pub summary: RoomSummary,
    pub members_synced: bool,

    pub encryption: Option<EncryptionEventContent>,
    pub last_prev_batch: Option<String>,
}

impl RoomInfo {
    pub fn mark_as_joined(&mut self) {
        self.room_type = RoomType::Joined;
    }

    pub fn mark_as_left(&mut self) {
        self.room_type = RoomType::Left;
    }

    pub fn mark_as_invited(&mut self) {
        self.room_type = RoomType::Invited;
    }

    pub fn mark_members_synced(&mut self) {
        self.members_synced = true;
    }

    pub fn mark_members_missing(&mut self) {
        self.members_synced = false;
    }

    pub fn set_prev_batch(&mut self, prev_batch: Option<&str>) -> bool {
        if self.last_prev_batch.as_deref() != prev_batch {
            self.last_prev_batch = prev_batch.map(|p| p.to_string());
            true
        } else {
            false
        }
    }

    pub fn handle_state_event(&mut self, event: &AnySyncStateEvent) -> bool {
        match event {
            AnySyncStateEvent::RoomEncryption(encryption) => {
                self.encryption = Some(encryption.content.clone());
                true
            }
            AnySyncStateEvent::RoomName(n) => {
                self.name = n.content.name().map(|n| n.to_string());
                true
            }
            AnySyncStateEvent::RoomCanonicalAlias(a) => {
                self.canonical_alias = a.content.alias.clone();
                true
            }
            AnySyncStateEvent::RoomTopic(t) => {
                self.topic = Some(t.content.topic.clone());
                true
            }
            _ => false,
        }
    }

    pub fn is_encrypted(&self) -> bool {
        self.encryption.is_some()
    }

    pub fn update_notification_count(&mut self, notification_counts: UnreadNotificationsCount) {
        self.notification_counts = notification_counts;
    }

    pub(crate) fn update(&mut self, summary: &RumaSummary) -> bool {
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
