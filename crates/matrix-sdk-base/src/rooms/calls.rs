// Copyright 2023 The Matrix.org Foundation C.I.C.
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

//! Things related to the room calls (also known as MatrixRTC sessions).

use std::{collections::HashMap, time::Duration};

use ruma::{
    events::call::member::SyncCallMemberEvent, MilliSecondsSinceUnixEpoch, OwnedDeviceId,
    OwnedUserId,
};
use serde::{Deserialize, Serialize};

/// Stores and handles the information related to the memberships of the calls
/// in a room.
///
/// Currently we only assume that we support `m.call` application types of the
/// calls and only the room calls (one such call per room).
#[derive(Debug, Default, Deserialize, Clone, Serialize)]
pub(crate) struct CallMembershipsHandler {
    /// Members of the call. We only store members of the active room call.
    memberships: HashMap<OwnedUserId, Vec<CallMember>>,
}

impl CallMembershipsHandler {
    /// Processes incoming sync call member event.
    pub(crate) fn handle_call_member_event(&mut self, event: &SyncCallMemberEvent) {
        let Some(event) = event.as_original().cloned() else {
            return;
        };

        let user_id = event.state_key;
        let user_members =
            event.content.memberships.into_iter().filter(|m| m.is_room_call()).map(|m| {
                CallMember {
                    device_id: m.device_id.into(),
                    info: CallMemberInfo {
                        member_since: m.created_ts.unwrap_or(event.origin_server_ts),
                        valid_for: m.expires,
                    },
                }
            });

        self.memberships.insert(user_id, user_members.collect());
    }

    /// Returns an active (on-going) room call.
    pub(crate) fn room_call(&self) -> Option<RoomCall> {
        let members: HashMap<_, _> = self
            .memberships
            .iter()
            .flat_map(|(user_id, devices)| {
                devices.iter().map(|device| {
                    let id = CallMemberIdentifier {
                        user_id: user_id.clone(),
                        device_id: device.device_id.clone(),
                    };

                    (id, device.info.clone())
                })
            })
            .filter(|(_, data)| !data.expired())
            .collect();

        (!members.is_empty()).then_some(RoomCall { members })
    }
}

/// Represents a room call (MatrixRTC session). A call consists of a set of
/// members who participate in a call. A call is active as long as there is at
/// least one member (participant) in it.
#[derive(Debug)]
pub struct RoomCall {
    /// The members (participants) of the call as of the moment of the last
    /// sync.
    pub(super) members: HashMap<CallMemberIdentifier, CallMemberInfo>,
}

impl RoomCall {
    /// Returns the time when the call was started, i.e. when the first known
    /// member joined.
    pub fn start_time(&self) -> MilliSecondsSinceUnixEpoch {
        self.members
            .values()
            .map(|member| member.member_since)
            .min()
            .expect("Bug: a room call must not be instantiated with 0 members")
    }

    /// Non-expired members of a call.
    pub fn active_members(&self) -> impl Iterator<Item = &CallMemberIdentifier> {
        self.members.iter().filter(|(_, member)| !member.expired()).map(|(id, _)| id)
    }
}

/// Info about a call member (participant).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CallMemberInfo {
    /// The time when the member joined the call.
    pub member_since: MilliSecondsSinceUnixEpoch,
    /// The duration for which the member is valid starting from
    /// [`Self::member_since`].
    pub valid_for: Duration,
}

impl CallMemberInfo {
    /// Returns `true` if the member is still valid, i.e. if the member joined
    /// and its membership has not expired yet. Returns `false` otherwise.
    pub fn expired(&self) -> bool {
        let expires_at = self.member_since.to_system_time().map(|start| start + self.valid_for);
        let now = MilliSecondsSinceUnixEpoch::now().to_system_time();
        expires_at.zip(now).map(|(expires_at, now)| expires_at < now).unwrap_or(true)
    }
}

/// A unique identifier of a call member (participant).
/// Each call member is identified by their user ID and device ID.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct CallMemberIdentifier {
    /// The user ID of the call member.
    pub user_id: OwnedUserId,
    /// The device ID of the call member.
    pub device_id: OwnedDeviceId,
}

/// Represents a single member of a call.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct CallMember {
    /// The member's device ID.
    device_id: OwnedDeviceId,
    /// Member info.
    info: CallMemberInfo,
}
