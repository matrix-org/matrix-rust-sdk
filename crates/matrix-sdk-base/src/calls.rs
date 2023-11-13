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

use std::{collections::BTreeMap, time::Duration};

use ruma::{MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId};

/// Represents a room call (MatrixRTC session). A call consists of a set of
/// members who participate in a call. A call is active as long as there is at
/// least one member (participant) in it.
#[derive(Debug)]
pub struct RoomCall {
    /// The members (participants) of the call as of the moment when the call
    /// was fetched.
    pub(crate) members: BTreeMap<CallMemberIdentifier, CallMemberData>,
}

impl RoomCall {
    /// Returns the time when the call was started, i.e. when the first member
    /// joined.
    ///
    /// Returns `None` if there are no members in the call (if the call is
    /// empty/ended).
    pub fn start_time(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        // TODO: We must take into an account that a call could have been started by a
        // participant who is no longer a member of a call.
        self.members.values().map(|member| member.member_since).min()
    }

    /// Non-expired members of a call.
    pub fn active_members(&self) -> impl Iterator<Item = &CallMemberIdentifier> {
        self.members.iter().filter(|(_, member)| !member.expired()).map(|(id, _)| id)
    }
}

/// A unique identifier of a call member (participant).
/// Each call member is identified by their user ID and device ID.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CallMemberIdentifier {
    /// The user ID of the call member.
    pub user_id: OwnedUserId,
    /// The device ID of the call member.
    pub device_id: OwnedDeviceId,
}

/// Data about a call member (participant).
#[derive(Clone, Debug)]
pub struct CallMemberData {
    /// The time when the member joined the call.
    pub member_since: MilliSecondsSinceUnixEpoch,
    /// The duration for which the member is valid starting from
    /// [`Self::member_since`].
    pub valid_for: Duration,
}

impl CallMemberData {
    /// Returns `true` if the member is still valid, i.e. if the member joined
    /// and its membership has not expired yet. Returns `false` otherwise.
    pub fn expired(&self) -> bool {
        let expires_at = self.member_since.to_system_time().map(|start| start + self.valid_for);
        let now = MilliSecondsSinceUnixEpoch::now().to_system_time();
        expires_at.zip(now).map(|(expires_at, now)| expires_at < now).unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ops::Sub,
        time::{Duration, SystemTime},
    };

    use ruma::{user_id, MilliSecondsSinceUnixEpoch};

    use super::{CallMemberData, CallMemberIdentifier, RoomCall};

    fn timestamp(seconds_ago: u64) -> MilliSecondsSinceUnixEpoch {
        let ts = SystemTime::now().sub(Duration::from_secs(seconds_ago));
        MilliSecondsSinceUnixEpoch::from_system_time(ts).unwrap()
    }

    #[test]
    fn call_start_time_equals_earliest_member_join_time() {
        let alice_ts = timestamp(30);
        let alice_id = CallMemberIdentifier {
            user_id: user_id!("@alice:server.name").to_owned(),
            device_id: "DEVICEID".into(),
        };
        let alice = CallMemberData { member_since: alice_ts, valid_for: Duration::from_secs(60) };

        let bob_ts = timestamp(25);
        let bob_id = CallMemberIdentifier {
            user_id: user_id!("@bob:other.server").to_owned(),
            device_id: "DEVICEID2".into(),
        };
        let bob = CallMemberData { member_since: bob_ts, valid_for: Duration::from_secs(60) };

        let john_ts = timestamp(65);
        let john_id = CallMemberIdentifier {
            user_id: user_id!("@expired:server.name").to_owned(),
            device_id: "DEVICEID3".into(),
        };
        let john = CallMemberData { member_since: john_ts, valid_for: Duration::from_secs(60) };

        let members = [(alice_id, alice), (bob_id, bob), (john_id, john)].into_iter().collect();
        let call = RoomCall { members };

        // John is the first particpant in a call (despite being the expired one). So we
        // must get the earliest timestamp here.
        assert_eq!(call.start_time(), Some(john_ts));

        // There are only 2 non-expired members in a call (john, the first user, should
        // have gotten expired by now).
        assert!(call.active_members().count() == 2);
    }
}
