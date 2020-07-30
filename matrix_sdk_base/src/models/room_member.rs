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

use matrix_sdk_common::{
    events::{presence::PresenceEvent, room::member::MemberEventContent, SyncStateEvent},
    identifiers::{RoomId, UserId},
    js_int::{Int, UInt},
    presence::PresenceState,
};
use serde::{Deserialize, Serialize};

// Notes: if Alice invites Bob into a room we will get an event with the sender as Alice and the state key as Bob.

#[derive(Debug, Serialize, Deserialize, Clone)]
/// A Matrix room member.
pub struct RoomMember {
    /// The unique MXID of the user.
    pub user_id: UserId,
    /// The human readable name of the user.
    pub display_name: Option<String>,
    /// Whether the member's display name is ambiguous due to being shared with
    /// other members.
    pub display_name_ambiguous: bool,
    /// The matrix url of the users avatar.
    pub avatar_url: Option<String>,
    /// The time, in ms, since the user interacted with the server.
    pub last_active_ago: Option<UInt>,
    /// If the user should be considered active.
    pub currently_active: Option<bool>,
    /// The unique id of the room.
    pub room_id: RoomId,
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
    /// The human readable name of this room member.
    pub name: String,
    // FIXME: The docstring below is currently a lie since we only store the initial event that
    // creates the member (the one we pass to RoomMember::new).
    //
    // The intent of this field is to keep the last (or last few?) state events related to the room
    // member cached so we can quickly go back to the previous one in case some of them get
    // redacted. Keeping all state for each room member is probably too much.
    //
    // Needs design.
    /// The events that created the state of this room member.
    pub events: Vec<SyncStateEvent<MemberEventContent>>,
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
            && self.display_name_ambiguous == other.display_name_ambiguous
            && self.avatar_url == other.avatar_url
            && self.last_active_ago == other.last_active_ago
    }
}

impl RoomMember {
    pub fn new(event: &SyncStateEvent<MemberEventContent>, room_id: &RoomId) -> Self {
        Self {
            name: event.state_key.clone(),
            room_id: room_id.clone(),
            user_id: UserId::try_from(event.state_key.as_str()).unwrap(),
            display_name: event.content.displayname.clone(),
            display_name_ambiguous: false,
            avatar_url: event.content.avatar_url.clone(),
            presence: None,
            status_msg: None,
            last_active_ago: None,
            currently_active: None,
            typing: None,
            power_level: None,
            power_level_norm: None,
            presence_events: Vec::default(),
            events: vec![event.clone()],
        }
    }

    /// Returns the most ergonomic (but potentially ambiguous/non-unique) name
    /// available for the member.
    ///
    /// This is the member's display name if it is set, otherwise their MXID.
    pub fn name(&self) -> String {
        self.display_name
            .clone()
            .unwrap_or_else(|| format!("{}", self.user_id))
    }

    /// Returns a name for the member which is guaranteed to be unique, but not
    /// necessarily the most ergonomic.
    ///
    /// This is either a name in the format "DISPLAY_NAME (MXID)" if the
    /// member's display name is set, or simply "MXID" if not.
    pub fn unique_name(&self) -> String {
        self.display_name
            .clone()
            .map(|d| format!("{} ({})", d, self.user_id))
            .unwrap_or_else(|| format!("{}", self.user_id))
    }

    /// Get the disambiguated display name for the member which is as ergonomic
    /// as possible while still guaranteeing it is unique.
    ///
    /// If the member's display name is currently ambiguous (i.e. shared by
    /// other room members), this method will return the same result as
    /// `RoomMember::unique_name`. Otherwise, this method will return the same
    /// result as `RoomMember::name`.
    ///
    /// This is usually the name you want when showing room messages from the
    /// member or when showing the member in the member list.
    ///
    /// **Warning**: When displaying a room member's display name, clients
    /// *must* use a disambiguated name, so they *must not* use
    /// `RoomMember::display_name` directly. Clients *should* use this method to
    /// obtain the name, but an acceptable alternative is to use
    /// `RoomMember::unique_name` in certain situations.
    pub fn disambiguated_name(&self) -> String {
        if self.display_name_ambiguous {
            self.unique_name()
        } else {
            self.name()
        }
    }
}

#[cfg(test)]
mod test {
    use matrix_sdk_test::{async_test, EventBuilder, EventsJson};

    use crate::{
        identifiers::{RoomId, UserId},
        BaseClient, Session,
    };

    use crate::js_int::int;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::*;

    use std::convert::TryFrom;

    async fn get_client() -> BaseClient {
        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".into(),
        };
        let client = BaseClient::new().unwrap();
        client.restore_login(session).await.unwrap();
        client
    }

    // TODO: Move this to EventBuilder since it's a magic room ID used in EventBuilder's example
    // events.
    fn test_room_id() -> RoomId {
        RoomId::try_from("!SVkFJHzfwvuaIEawgC:localhost").unwrap()
    }

    #[async_test]
    async fn room_member_events() {
        let client = get_client().await;

        let room_id = test_room_id();

        let mut response = EventBuilder::default()
            .add_room_event(EventsJson::Member)
            .add_room_event(EventsJson::PowerLevels)
            .build_sync_response();

        client.receive_sync_response(&mut response).await.unwrap();

        let room = client.get_joined_room(&room_id).await.unwrap();
        let room = room.read().await;

        let member = room
            .joined_members
            .get(&UserId::try_from("@example:localhost").unwrap())
            .unwrap();
        assert_eq!(member.power_level, Some(int!(100)));
    }

    #[async_test]
    async fn room_member_display_name_change() {
        let client = get_client().await;
        let room_id = test_room_id();

        let mut builder = EventBuilder::default();
        let mut initial_response = builder
            .add_room_event(EventsJson::Member)
            .build_sync_response();
        let mut name_change_response = builder
            .add_room_event(EventsJson::MemberNameChange)
            .build_sync_response();

        client
            .receive_sync_response(&mut initial_response)
            .await
            .unwrap();

        let room = client.get_joined_room(&room_id).await.unwrap();

        // Initially, the display name is "example".
        {
            let room = room.read().await;

            let member = room
                .joined_members
                .get(&UserId::try_from("@example:localhost").unwrap())
                .unwrap();

            assert_eq!(member.display_name.as_ref().unwrap(), "example");
        }

        client
            .receive_sync_response(&mut name_change_response)
            .await
            .unwrap();

        // Afterwards, the display name is "changed".
        {
            let room = room.read().await;

            let member = room
                .joined_members
                .get(&UserId::try_from("@example:localhost").unwrap())
                .unwrap();

            assert_eq!(member.display_name.as_ref().unwrap(), "changed");
        }
    }

    #[async_test]
    async fn member_presence_events() {
        let client = get_client().await;

        let room_id = test_room_id();

        let mut response = EventBuilder::default()
            .add_room_event(EventsJson::Member)
            .add_room_event(EventsJson::PowerLevels)
            .add_presence_event(EventsJson::Presence)
            .build_sync_response();

        client.receive_sync_response(&mut response).await.unwrap();

        let room = client.get_joined_room(&room_id).await.unwrap();
        let room = room.read().await;

        let member = room
            .joined_members
            .get(&UserId::try_from("@example:localhost").unwrap())
            .unwrap();

        assert_eq!(member.power_level, Some(int!(100)));

        assert!(member.avatar_url.is_none());
        assert_eq!(member.last_active_ago, None);
        assert_eq!(member.presence, None);
    }
}
