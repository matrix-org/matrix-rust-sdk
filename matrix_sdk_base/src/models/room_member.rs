// Copyright 2020 Damir Jelić
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

use std::convert::TryFrom;

use crate::events::presence::{PresenceEvent, PresenceEventContent, PresenceState};
use crate::events::room::{
    member::{MemberEventContent, MembershipChange, MembershipState},
    power_levels::PowerLevelsEventContent,
};
use crate::events::StateEventStub;
use crate::identifiers::{RoomId, UserId};

use crate::js_int::{Int, UInt};
use serde::{Deserialize, Serialize};
// Notes: if Alice invites Bob into a room we will get an event with the sender as Alice and the state key as Bob.

#[derive(Debug, Serialize, Deserialize, Clone)]
/// A Matrix room member.
pub struct RoomMember {
    /// The unique MXID of the user.
    pub user_id: UserId,
    /// The human readable name of the user.
    pub display_name: Option<String>,
    /// The matrix url of the users avatar.
    pub avatar_url: Option<String>,
    /// The time, in ms, since the user interacted with the server.
    pub last_active_ago: Option<UInt>,
    /// If the user should be considered active.
    pub currently_active: Option<bool>,
    /// The unique id of the room.
    pub room_id: Option<RoomId>,
    /// If the member is typing.
    pub typing: Option<bool>,
    /// The presence of the user, if found.
    pub presence: Option<PresenceState>,
    /// The presence status message, if found.
    pub status_msg: Option<String>,
    /// The users power level.
    pub power_level: Option<Int>,
    /// The normalized power level of this `RoomMember` (0-100).
    pub power_level_norm: Option<Int>,
    /// The `MembershipState` of this `RoomMember`.
    pub membership: MembershipState,
    /// The human readable name of this room member.
    pub name: String,
    /// The `PresenceEvent`s connected to this user.
    pub presence_events: Vec<PresenceEvent>,
}

impl PartialEq for RoomMember {
    fn eq(&self, other: &RoomMember) -> bool {
        // TODO check everything but events and presence_events they don't impl PartialEq
        self.room_id == other.room_id
            && self.user_id == other.user_id
            && self.name == other.name
            && self.display_name == other.display_name
            && self.avatar_url == other.avatar_url
            && self.last_active_ago == other.last_active_ago
    }
}

impl RoomMember {
    pub fn new(event: &StateEventStub<MemberEventContent>, room_id: &RoomId) -> Self {
        Self {
            name: event.state_key.clone(),
            room_id: Some(room_id.clone()),
            user_id: UserId::try_from(event.state_key.as_str()).unwrap(),
            display_name: event.content.displayname.clone(),
            avatar_url: event.content.avatar_url.clone(),
            presence: None,
            status_msg: None,
            last_active_ago: None,
            currently_active: None,
            typing: None,
            power_level: None,
            power_level_norm: None,
            membership: event.content.membership,
            presence_events: vec![],
        }
    }

    /// Returns the most ergonomic name available for the member.
    ///
    /// This is the member's display name if it is set, otherwise their MXID.
    pub fn name(&self) -> String {
        self.display_name
            .clone()
            .unwrap_or_else(|| format!("{}", self.user_id))
    }

    /// Returns a name for the member which is guaranteed to be unique.
    ///
    /// This is either of the format "DISPLAY_NAME (MXID)" if the display name is set for the
    /// member, or simply "MXID" if not.
    pub fn unique_name(&self) -> String {
        self.display_name
            .clone()
            .map(|d| format!("{} ({})", d, self.user_id))
            .unwrap_or_else(|| format!("{}", self.user_id))
    }

    /// Handle profile updates.
    pub(crate) fn update_profile(&mut self, event: &StateEventStub<MemberEventContent>) -> bool {
        use MembershipChange::*;

        match event.membership_change() {
            ProfileChanged => {
                self.display_name = event.content.displayname.clone();
                self.avatar_url = event.content.avatar_url.clone();
                true
            }

            // We're only interested in profile changes here.
            _ => false,
        }
    }

    pub fn update_power(
        &mut self,
        event: &StateEventStub<PowerLevelsEventContent>,
        max_power: Int,
    ) -> bool {
        let changed;
        if let Some(user_power) = event.content.users.get(&self.user_id) {
            changed = self.power_level != Some(*user_power);
            self.power_level = Some(*user_power);
        } else {
            changed = self.power_level != Some(event.content.users_default);
            self.power_level = Some(event.content.users_default);
        }

        if max_power > Int::from(0) {
            self.power_level_norm = Some((self.power_level.unwrap() * Int::from(100)) / max_power);
        }

        changed
    }

    /// If the current `PresenceEvent` updated the state of this `User`.
    ///
    /// Returns true if the specific users presence has changed, false otherwise.
    ///
    /// # Arguments
    ///
    /// * `presence` - The presence event for a this room member.
    pub fn did_update_presence(&self, presence: &PresenceEvent) -> bool {
        let PresenceEvent {
            content:
                PresenceEventContent {
                    avatar_url,
                    currently_active,
                    displayname,
                    last_active_ago,
                    presence,
                    status_msg,
                },
            ..
        } = presence;
        self.display_name == *displayname
            && self.avatar_url == *avatar_url
            && self.presence.as_ref() == Some(presence)
            && self.status_msg == *status_msg
            && self.last_active_ago == *last_active_ago
            && self.currently_active == *currently_active
    }

    /// Updates the `User`s presence.
    ///
    /// This should only be used if `did_update_presence` was true.
    ///
    /// # Arguments
    ///
    /// * `presence` - The presence event for a this room member.
    pub fn update_presence(&mut self, presence_ev: &PresenceEvent) {
        let PresenceEvent {
            content:
                PresenceEventContent {
                    avatar_url,
                    currently_active,
                    displayname,
                    last_active_ago,
                    presence,
                    status_msg,
                },
            ..
        } = presence_ev;

        self.presence_events.push(presence_ev.clone());
        self.avatar_url = avatar_url.clone();
        self.currently_active = *currently_active;
        self.display_name = displayname.clone();
        self.last_active_ago = *last_active_ago;
        self.presence = Some(*presence);
        self.status_msg = status_msg.clone();
    }
}

#[cfg(test)]
mod test {
    use matrix_sdk_test::{async_test, EventBuilder, EventsJson};

    use crate::events::room::member::MembershipState;
    use crate::identifiers::{RoomId, UserId};
    use crate::{BaseClient, Session};

    use crate::js_int::Int;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::*;

    use std::convert::TryFrom;

    async fn get_client() -> BaseClient {
        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };
        let client = BaseClient::new().unwrap();
        client.restore_login(session).await.unwrap();
        client
    }

    fn get_room_id() -> RoomId {
        RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap()
    }

    #[async_test]
    async fn room_member_events() {
        let client = get_client().await;

        let room_id = get_room_id();

        let mut response = EventBuilder::default()
            .add_state_event(EventsFile::Member)
            .add_state_event(EventsFile::PowerLevels)
            .build_sync_response();

        client.receive_sync_response(&mut response).await.unwrap();

        let room = client.get_joined_room(&room_id).await.unwrap();
        let room = room.read().await;

        let member = room
            .joined_members
            .get(&UserId::try_from("@example:localhost").unwrap())
            .unwrap();
        assert_eq!(member.power_level, Int::new(100));
    }

    #[async_test]
    async fn member_presence_events() {
        let client = get_client().await;

        let room_id = get_room_id();

        let mut response = EventBuilder::default()
            .add_state_event(EventsFile::Member)
            .add_state_event(EventsFile::PowerLevels)
            .add_presence_event(EventsFile::Presence)
            .build_sync_response();

        client.receive_sync_response(&mut response).await.unwrap();

        let room = client.get_joined_room(&room_id).await.unwrap();
        let room = room.read().await;

        let member = room
            .joined_members
            .get(&UserId::try_from("@example:localhost").unwrap())
            .unwrap();

        assert_eq!(member.power_level, Int::new(100));

        assert!(member.avatar_url.is_none());
        assert_eq!(member.last_active_ago, None);
        assert_eq!(member.presence, None);
    }
}
