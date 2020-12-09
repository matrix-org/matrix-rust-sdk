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

#[derive(Clone, Debug)]
pub struct RoomMember {
    pub(crate) user_id: Arc<UserId>,
    pub(crate) event: Arc<SyncStateEvent<MemberEventContent>>,
    pub(crate) presence: Arc<Option<PresenceEvent>>,
    pub(crate) power_levles: Arc<Option<SyncStateEvent<PowerLevelsEventContent>>>,
}

impl RoomMember {
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    pub fn display_name(&self) -> Option<&str> {
        self.event.content.displayname.as_deref()
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
                    .unwrap_or(e.content.users_default.into())
            })
            .unwrap_or(0)
    }
}
