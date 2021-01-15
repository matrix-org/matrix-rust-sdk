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

use std::sync::Arc;

use matrix_sdk_common::{
    events::{
        presence::PresenceEvent,
        room::{member::MemberEventContent, power_levels::PowerLevelsEventContent},
        SyncStateEvent,
    },
    identifiers::UserId,
};

use crate::responses::MemberEvent;

#[derive(Clone, Debug)]
pub struct RoomMember {
    pub(crate) event: Arc<MemberEvent>,
    pub(crate) profile: Arc<Option<MemberEventContent>>,
    pub(crate) presence: Arc<Option<PresenceEvent>>,
    pub(crate) power_levles: Arc<Option<SyncStateEvent<PowerLevelsEventContent>>>,
    pub(crate) max_power_level: i64,
    pub(crate) is_room_creator: bool,
}

impl RoomMember {
    pub fn user_id(&self) -> &UserId {
        &self.event.state_key
    }

    pub fn display_name(&self) -> Option<&str> {
        if let Some(p) = self.profile.as_ref() {
            p.displayname.as_deref()
        } else {
            self.event.content.displayname.as_deref()
        }
    }

    pub fn name(&self) -> &str {
        if let Some(d) = self.display_name() {
            d
        } else {
            self.user_id().localpart()
        }
    }

    pub fn avatar_url(&self) -> Option<&str> {
        match self.profile.as_ref() {
            Some(p) => p.avatar_url.as_deref(),
            None => self.event.content.avatar_url.as_deref(),
        }
    }

    pub fn normalized_power_level(&self) -> i64 {
        if self.max_power_level > 0 {
            (self.power_level() * 100) / self.max_power_level
        } else {
            self.power_level()
        }
    }

    pub fn power_level(&self) -> i64 {
        self.power_levles
            .as_ref()
            .as_ref()
            .map(|e| {
                e.content
                    .users
                    .get(&self.user_id())
                    .map(|p| (*p).into())
                    .unwrap_or_else(|| e.content.users_default.into())
            })
            .unwrap_or_else(|| if self.is_room_creator { 100 } else { 0 })
    }
}
