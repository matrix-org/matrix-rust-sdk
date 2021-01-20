// Copyright 2021 The Matrix.org Foundation C.I.C.
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

mod store_key;

use std::{convert::TryFrom, path::Path, time::SystemTime};

use futures::stream::{self, Stream};
use matrix_sdk_common::{
    events::{
        presence::PresenceEvent,
        room::member::{MemberEventContent, MembershipState},
        AnySyncStateEvent, EventContent, EventType,
    },
    identifiers::{RoomId, UserId},
};

use sled::{
    transaction::{ConflictableTransactionError, TransactionError},
    Config, Db, Transactional, Tree,
};
use tracing::info;

use crate::deserialized_responses::MemberEvent;

use super::{Result, RoomInfo, StateChanges, StoreError};

#[derive(Debug, Clone)]
pub struct SledStore {
    inner: Db,
    session: Tree,
    account_data: Tree,
    members: Tree,
    profiles: Tree,
    joined_user_ids: Tree,
    invited_user_ids: Tree,
    room_info: Tree,
    room_state: Tree,
    room_account_data: Tree,
    stripped_room_info: Tree,
    stripped_room_state: Tree,
    stripped_members: Tree,
    presence: Tree,
}

impl SledStore {
    fn open_helper(db: Db) -> Result<Self> {
        let session = db.open_tree("session")?;
        let account_data = db.open_tree("account_data")?;

        let members = db.open_tree("members")?;
        let profiles = db.open_tree("profiles")?;
        let joined_user_ids = db.open_tree("joined_user_ids")?;
        let invited_user_ids = db.open_tree("invited_user_ids")?;

        let room_state = db.open_tree("room_state")?;
        let room_info = db.open_tree("room_infos")?;
        let presence = db.open_tree("presence")?;
        let room_account_data = db.open_tree("room_account_data")?;

        let stripped_room_info = db.open_tree("stripped_room_info")?;
        let stripped_members = db.open_tree("stripped_members")?;
        let stripped_room_state = db.open_tree("stripped_room_state")?;

        Ok(Self {
            inner: db,
            session,
            account_data,
            members,
            profiles,
            joined_user_ids,
            invited_user_ids,
            room_account_data,
            presence,
            room_state,
            room_info,
            stripped_room_info,
            stripped_members,
            stripped_room_state,
        })
    }

    pub fn open() -> Result<Self> {
        let db = Config::new().temporary(true).open()?;

        SledStore::open_helper(db)
    }

    pub fn open_with_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().join("matrix-sdk-state");
        let db = Config::new().temporary(false).path(path).open()?;

        SledStore::open_helper(db)
    }

    pub async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        self.session
            .insert(&format!("filter{}", filter_name), filter_id)?;

        Ok(())
    }

    pub async fn get_filter(&self, filter_name: &str) -> Result<Option<String>> {
        Ok(self
            .session
            .get(&format!("filter{}", filter_name))?
            .map(|f| String::from_utf8_lossy(&f).to_string()))
    }

    pub async fn get_sync_token(&self) -> Result<Option<String>> {
        Ok(self
            .session
            .get("sync_token")?
            .map(|t| String::from_utf8_lossy(&t).to_string()))
    }

    pub async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let now = SystemTime::now();

        let ret: std::result::Result<(), TransactionError<serde_json::Error>> = (
            &self.session,
            &self.account_data,
            &self.members,
            &self.profiles,
            &self.joined_user_ids,
            &self.invited_user_ids,
            &self.room_info,
            &self.room_state,
            &self.room_account_data,
            &self.presence,
            &self.stripped_room_info,
            &self.stripped_members,
            &self.stripped_room_state,
        )
            .transaction(
                |(
                    session,
                    account_data,
                    members,
                    profiles,
                    joined,
                    invited,
                    rooms,
                    state,
                    room_account_data,
                    presence,
                    striped_rooms,
                    stripped_members,
                    stripped_state,
                )| {
                    if let Some(s) = &changes.sync_token {
                        session.insert("sync_token", s.as_str())?;
                    }

                    for (room, events) in &changes.members {
                        for event in events.values() {
                            let key = format!("{}{}", room.as_str(), event.state_key.as_str());

                            match event.content.membership {
                                MembershipState::Join => {
                                    joined.insert(key.as_str(), event.state_key.as_str())?;
                                    invited.remove(key.as_str())?;
                                }
                                MembershipState::Invite => {
                                    invited.insert(key.as_str(), event.state_key.as_str())?;
                                    joined.remove(key.as_str())?;
                                }
                                _ => {
                                    joined.remove(key.as_str())?;
                                    invited.remove(key.as_str())?;
                                }
                            }

                            members.insert(
                                format!("{}{}", room.as_str(), &event.state_key).as_str(),
                                serde_json::to_vec(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, users) in &changes.profiles {
                        for (user_id, profile) in users {
                            profiles.insert(
                                format!("{}{}", room.as_str(), user_id.as_str()).as_str(),
                                serde_json::to_vec(&profile)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (event_type, event) in &changes.account_data {
                        account_data.insert(
                            event_type.as_str(),
                            serde_json::to_vec(&event)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room, events) in &changes.room_account_data {
                        for (event_type, event) in events {
                            room_account_data.insert(
                                format!("{}{}", room.as_str(), event_type).as_str(),
                                serde_json::to_vec(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, event_types) in &changes.state {
                        for events in event_types.values() {
                            for event in events.values() {
                                state.insert(
                                    format!(
                                        "{}{}{}",
                                        room.as_str(),
                                        event.content().event_type(),
                                        event.state_key(),
                                    )
                                    .as_bytes(),
                                    serde_json::to_vec(&event)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    for (room_id, room_info) in &changes.room_infos {
                        rooms.insert(
                            room_id.as_bytes(),
                            serde_json::to_vec(room_info)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (sender, event) in &changes.presence {
                        presence.insert(
                            sender.as_bytes(),
                            serde_json::to_vec(&event)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room_id, info) in &changes.invited_room_info {
                        striped_rooms.insert(
                            room_id.as_str(),
                            serde_json::to_vec(&info)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (room, events) in &changes.stripped_members {
                        for event in events.values() {
                            stripped_members.insert(
                                format!("{}{}", room.as_str(), &event.state_key).as_str(),
                                serde_json::to_vec(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, event_types) in &changes.stripped_state {
                        for events in event_types.values() {
                            for event in events.values() {
                                stripped_state.insert(
                                    format!(
                                        "{}{}{}",
                                        room.as_str(),
                                        event.content().event_type(),
                                        event.state_key(),
                                    )
                                    .as_bytes(),
                                    serde_json::to_vec(&event)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    Ok(())
                },
            );

        ret?;

        self.inner.flush_async().await?;

        info!("Saved changes in {:?}", now.elapsed());

        Ok(())
    }

    pub async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<PresenceEvent>> {
        Ok(self
            .presence
            .get(user_id.as_bytes())?
            .map(|e| serde_json::from_slice(&e))
            .transpose()?)
    }

    pub async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<AnySyncStateEvent>> {
        Ok(self
            .room_state
            .get(format!("{}{}{}", room_id.as_str(), event_type, state_key).as_bytes())?
            .map(|e| serde_json::from_slice(&e))
            .transpose()?)
    }

    pub async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MemberEventContent>> {
        Ok(self
            .profiles
            .get(format!("{}{}", room_id.as_str(), user_id.as_str()))?
            .map(|p| serde_json::from_slice(&p))
            .transpose()?)
    }

    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        Ok(self
            .members
            .get(format!("{}{}", room_id.as_str(), state_key.as_str()))?
            .map(|v| serde_json::from_slice(&v))
            .transpose()?)
    }

    pub async fn get_invited_user_ids(
        &self,
        room_id: &RoomId,
    ) -> impl Stream<Item = Result<UserId>> {
        stream::iter(
            self.invited_user_ids
                .scan_prefix(room_id.as_bytes())
                .map(|u| {
                    UserId::try_from(String::from_utf8_lossy(&u?.1).to_string())
                        .map_err(StoreError::Identifier)
                }),
        )
    }

    pub async fn get_joined_user_ids(
        &self,
        room_id: &RoomId,
    ) -> impl Stream<Item = Result<UserId>> {
        stream::iter(
            self.joined_user_ids
                .scan_prefix(room_id.as_bytes())
                .map(|u| {
                    UserId::try_from(String::from_utf8_lossy(&u?.1).to_string())
                        .map_err(StoreError::Identifier)
                }),
        )
    }

    pub async fn get_room_infos(&self) -> impl Stream<Item = Result<RoomInfo>> {
        stream::iter(
            self.room_info
                .iter()
                .map(|r| serde_json::from_slice(&r?.1).map_err(StoreError::Json)),
        )
    }
}

#[cfg(test)]
mod test {
    use std::{convert::TryFrom, time::SystemTime};

    use matrix_sdk_common::{
        events::{
            room::member::{MemberEventContent, MembershipState},
            Unsigned,
        },
        identifiers::{room_id, user_id, EventId, UserId},
    };
    use matrix_sdk_test::async_test;

    use super::{SledStore, StateChanges};
    use crate::deserialized_responses::MemberEvent;

    fn user_id() -> UserId {
        user_id!("@example:localhost")
    }

    fn membership_event() -> MemberEvent {
        let content = MemberEventContent {
            avatar_url: None,
            displayname: None,
            is_direct: None,
            third_party_invite: None,
            membership: MembershipState::Join,
        };

        MemberEvent {
            event_id: EventId::try_from("$h29iv0s8:example.com").unwrap(),
            content,
            sender: user_id(),
            origin_server_ts: SystemTime::now(),
            state_key: user_id(),
            prev_content: None,
            unsigned: Unsigned::default(),
        }
    }

    #[async_test]
    async fn test_member_saving() {
        let store = SledStore::open().unwrap();
        let room_id = room_id!("!test:localhost");
        let user_id = user_id();

        assert!(store
            .get_member_event(&room_id, &user_id)
            .await
            .unwrap()
            .is_none());
        let mut changes = StateChanges::default();
        changes
            .members
            .entry(room_id.clone())
            .or_default()
            .insert(user_id.clone(), membership_event());

        store.save_changes(&changes).await.unwrap();
        assert!(store
            .get_member_event(&room_id, &user_id)
            .await
            .unwrap()
            .is_some());
    }
}
