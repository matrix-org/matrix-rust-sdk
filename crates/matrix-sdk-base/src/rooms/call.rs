// Copyright 2023 The Matrix.org Foundation C.I.C.

//! Things related to the active room call (also known as MatrixRTC session).

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use ruma::{
    events::{call::member::CallMemberEventContent, OriginalSyncStateEvent},
    MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId,
};
use serde::{Deserialize, Serialize};

/// Represents a room call (MatrixRTC session). A call consists of a set of
/// members who participate in a call. A call is active as long as there is at
/// least one member (participant) in it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RoomCall {
    /// The members (participants) of a call.
    pub members: HashMap<CallMemberIdentifier, CallMemberInfo>,
    /// The time when the call was started.
    pub start_time: MilliSecondsSinceUnixEpoch,
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

/// Info about a call member (participant).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

/// Represents a single member of a call.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CallMember {
    /// The member's device ID.
    pub device_id: OwnedDeviceId,
    /// Member info.
    pub info: CallMemberInfo,
}

/// Mixin that provides functions to allow the extraction
/// of some useful information from a `CallMemberEvent`.
pub trait CallMemberEventExt {
    /// Returns the call members described by this event.
    fn current_memberships(&self) -> Vec<CallMember>;
    /// Returns the call members before this event (unsigned field of a state
    /// event).
    fn previous_memberships(&self) -> Option<Vec<CallMember>>;
    /// Each call member contains the list of calls that any given user
    /// participates in. The diff in this context is a structure that
    /// contains the calls that a given user joined or left as a result of
    /// this event (i.e. if this call event states that a user is part of a
    /// call X and the previous state event does not contain this
    /// information, then this user joined the call X as a result of this
    /// event).
    fn diff(&self) -> CallMemberDiff {
        let new: HashSet<_> = self.current_memberships().into_iter().map(|m| m.device_id).collect();
        let old: HashSet<_> = self
            .previous_memberships()
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.device_id)
            .collect();

        let joined = new.difference(&old).cloned().collect();
        let left = old.difference(&new).cloned().collect();
        CallMemberDiff { joined, left }
    }
}

impl CallMemberEventExt for OriginalSyncStateEvent<CallMemberEventContent> {
    fn current_memberships(&self) -> Vec<CallMember> {
        self.content
            .clone()
            .memberships
            .into_iter()
            .filter(|m| m.is_room_call())
            .map(|m| CallMember {
                device_id: m.device_id.into(),
                info: CallMemberInfo {
                    member_since: m.created_ts.unwrap_or(self.origin_server_ts),
                    valid_for: m.expires,
                },
            })
            .collect()
    }

    fn previous_memberships(&self) -> Option<Vec<CallMember>> {
        Some(
            self.unsigned
                .prev_content
                .clone()
                .and_then(|prev_content| prev_content.memberships)?
                .into_iter()
                .filter(|m| m.is_room_call())
                .map(|m| CallMember {
                    device_id: m.device_id.into(),
                    info: CallMemberInfo {
                        member_since: m.created_ts.unwrap_or(self.origin_server_ts),
                        valid_for: m.expires,
                    },
                })
                .collect(),
        )
    }
}

/// Contains information extracted out of a given `CallMemberEvent`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CallMemberDiff {
    /// Calls that certain devices of this user joined.
    pub joined: Vec<OwnedDeviceId>,
    /// Calls that certain devices of this user left.
    pub left: Vec<OwnedDeviceId>,
}
