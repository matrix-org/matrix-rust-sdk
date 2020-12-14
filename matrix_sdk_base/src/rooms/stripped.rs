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

use std::sync::{Arc, Mutex as SyncMutex};

use matrix_sdk_common::{
    events::AnyStrippedStateEvent,
    identifiers::{RoomId, UserId},
};
use serde::{Deserialize, Serialize};

use crate::store::Store;

use super::BaseRoomInfo;

#[derive(Debug, Clone)]
pub struct StrippedRoom {
    room_id: Arc<RoomId>,
    own_user_id: Arc<UserId>,
    inner: Arc<SyncMutex<StrippedRoomInfo>>,
    store: Store,
}

impl StrippedRoom {
    pub fn new(own_user_id: &UserId, store: Store, room_id: &RoomId) -> Self {
        let room_id = Arc::new(room_id.clone());

        Self {
            own_user_id: Arc::new(own_user_id.clone()),
            room_id: room_id.clone(),
            store,
            inner: Arc::new(SyncMutex::new(StrippedRoomInfo {
                room_id,
                base_info: BaseRoomInfo::new(),
            })),
        }
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

        if let Some(name) = &inner.base_info.name {
            let name = name.trim();
            name.to_string()
        } else if let Some(alias) = &inner.base_info.canonical_alias {
            let alias = alias.alias().trim();
            alias.to_string()
        } else {
            todo!()
            // let joined = inner.summary.joined_member_count;
            // let invited = inner.summary.invited_member_count;
            // let heroes_count = inner.summary.heroes.len() as u64;
            // let invited_joined = (invited + joined).saturating_sub(1);

            // let members = self.get_j_members().await;

            // info!(
            //     "Calculating name for {}, hero count {} members {:#?}",
            //     self.room_id(),
            //     heroes_count,
            //     members
            // );
            // TODO: This should use `self.heroes` but it is always empty??
            //
            // let own_user_id = self.own_user_id.clone();

            // let is_own_member = |m: &RoomMember| m.user_id() == &*own_user_id;

            // if !inner.summary.heroes.is_empty() {
            //     let mut names = stream::iter(inner.summary.heroes.iter())
            //         .take(3)
            //         .filter_map(|u| async move {
            //             let user_id = UserId::try_from(u.as_str()).ok()?;
            //             self.get_member(&user_id).await
            //         })
            //         .map(|mem| {
            //             mem.display_name()
            //                 .map(|d| d.to_string())
            //                 .unwrap_or_else(|| mem.user_id().localpart().to_string())
            //         })
            //         .collect::<Vec<String>>()
            //         .await;
            //     names.sort();
            //     names.join(", ")
            // } else if heroes_count >= invited_joined {
            //     let mut names = members
            //         .filter(|m| future::ready(is_own_member(m)))
            //         .take(3)
            //         .map(|mem| {
            //             mem.display_name()
            //                 .map(|d| d.to_string())
            //                 .unwrap_or_else(|| mem.user_id().localpart().to_string())
            //         })
            //         .collect::<Vec<String>>()
            //         .await;
            //     // stabilize ordering
            //     names.sort();
            //     names.join(", ")
            // } else if heroes_count < invited_joined && invited + joined > 1 {
            //     let mut names = members
            //         .filter(|m| future::ready(is_own_member(m)))
            //         .take(3)
            //         .map(|mem| {
            //             mem.display_name()
            //                 .map(|d| d.to_string())
            //                 .unwrap_or_else(|| mem.user_id().localpart().to_string())
            //         })
            //         .collect::<Vec<String>>()
            //         .await;
            //     names.sort();

            //     // TODO: What length does the spec want us to use here and in
            //     // the `else`?
            //     format!("{}, and {} others", names.join(", "), (joined + invited))
            // } else {
            //     "Empty room".to_string()
            // }
        }
    }

    pub fn own_user_id(&self) -> &UserId {
        &self.own_user_id
    }

    pub(crate) fn clone_info(&self) -> StrippedRoomInfo {
        (*self.inner.lock().unwrap()).clone()
    }

    pub fn is_encrypted(&self) -> bool {
        self.inner.lock().unwrap().base_info.encryption.is_some()
    }

    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    pub async fn display_name(&self) -> String {
        self.calculate_name().await
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StrippedRoomInfo {
    pub room_id: Arc<RoomId>,
    pub base_info: BaseRoomInfo,
}

impl StrippedRoomInfo {
    pub(crate) fn handle_state_event(&mut self, event: &AnyStrippedStateEvent) -> bool {
        self.base_info.handle_state_event(&event.content())
    }
}
