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

use crate::events::collections::all::Event;
use crate::events::room::{
    member::{MemberEvent, MembershipChange, MembershipState},
    power_levels::PowerLevelsEvent,
};
use crate::events::presence::{PresenceEvent, PresenceEventContent, PresenceState};
use crate::identifiers::UserId;

use js_int::{Int, UInt};

// Notes: if Alice invites Bob into a room we will get an event with the sender as Alice and the state key as Bob.

#[derive(Debug)]
/// A Matrix room member.
///
pub struct RoomMember {
    /// The unique mxid of the user.
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
    pub room_id: Option<String>,
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
    /// The events that created the state of this room member.
    pub events: Vec<Event>,
    /// The `PresenceEvent`s connected to this user.
    pub presence_events: Vec<PresenceEvent>,
}

impl RoomMember {
    pub fn new(event: &MemberEvent) -> Self {
        Self {
            name: event.state_key.clone(),
            room_id: event.room_id.as_ref().map(|id| id.to_string()),
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
            presence_events: Vec::default(),
            events: vec![Event::RoomMember(event.clone())],
        }
    }

    pub fn update_member(&mut self, event: &MemberEvent) -> bool {
        use MembershipChange::*;

        match event.membership_change() {
            ProfileChanged => {
                self.display_name = event.content.displayname.clone();
                self.avatar_url = event.content.avatar_url.clone();
                true
            }
            Banned | Kicked | KickedAndBanned | InvitationRejected | InvitationRevoked | Left
            | Unbanned | Joined | Invited => {
                self.membership = event.content.membership;
                true
            }
            NotImplemented => false,
            None => false,
            // we ignore the error here as only a buggy or malicious server would send this
            Error => false,
            _ => false,
        }
    }

    pub fn update_power(&mut self, event: &PowerLevelsEvent) -> bool {
        let mut max_power = event.content.users_default;
        for power in event.content.users.values() {
            max_power = *power.max(&max_power);
        }

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
        self.currently_active = currently_active.clone();
        self.display_name = displayname.clone();
        self.last_active_ago = last_active_ago.clone();
        self.presence = Some(presence.clone());
        self.status_msg = status_msg.clone();
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::identifiers::{EventId, RoomId, UserId};
    use crate::{AsyncClient, Session, SyncSettings};

    use serde_json;

    use js_int::{Int, UInt};
    use mockito::{mock, Matcher};
    use url::Url;

    use std::collections::HashMap;
    use std::convert::TryFrom;
    use std::str::FromStr;
    use std::time::Duration;

    use crate::events::room::power_levels::{
        NotificationPowerLevels, PowerLevelsEvent, PowerLevelsEventContent,
    };

    #[tokio::test]
    async fn member_power() {
        let homeserver = Url::from_str(&mockito::server_url()).unwrap();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
        )
        .with_status(200)
        .with_body_from_file("tests/data/sync.json")
        .create();

        let mut client = AsyncClient::new(homeserver, Some(session)).unwrap();

        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let _response = client.sync(sync_settings).await.unwrap();

        let mut rooms = client.base_client.write().await.joined_rooms.clone();
        let mut room = rooms
            .get_mut(&RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap())
            .unwrap()
            .lock()
            .await;

        for (_id, member) in &mut room.members {
            let power = power_levels();
            assert!(member.update_power(&power));
            assert_eq!(MembershipState::Join, member.membership);
        }
    }

    fn power_levels() -> PowerLevelsEvent {
        PowerLevelsEvent {
            content: PowerLevelsEventContent {
                ban: Int::new(40).unwrap(),
                events: HashMap::default(),
                events_default: Int::new(40).unwrap(),
                invite: Int::new(40).unwrap(),
                kick: Int::new(40).unwrap(),
                redact: Int::new(40).unwrap(),
                state_default: Int::new(40).unwrap(),
                users: HashMap::default(),
                users_default: Int::new(40).unwrap(),
                notifications: NotificationPowerLevels {
                    room: Int::new(35).unwrap(),
                },
            },
            event_id: EventId::try_from("$h29iv0s8:example.com").unwrap(),
            origin_server_ts: UInt::new(1520372800469).unwrap(),
            prev_content: None,
            room_id: RoomId::try_from("!roomid:room.com").ok(),
            unsigned: serde_json::Map::new(),
            sender: UserId::try_from("@example:example.com").unwrap(),
            state_key: "@example:example.com".into(),
        }
    }
}
