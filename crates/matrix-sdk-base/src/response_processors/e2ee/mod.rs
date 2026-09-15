// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use matrix_sdk_crypto::{DecryptionSettings, OlmMachine};
use ruma::{
    OwnedUserId,
    events::{
        AnySyncStateEvent, StateEventType,
        room::member::{MembershipState, RoomMemberEventContent},
    },
};
use serde::Deserialize;
use tracing::warn;

use super::Context;
use crate::{Room, RoomState, room::RoomInfoNotableUpdateReasons, utils::RawStateEventWithKeys};

pub mod decrypt;
pub mod to_device;
pub mod tracked_users;

/// If a sync response transitions the current user's membership in `room` to
/// joined WITHOUT this client having performed the join — e.g. the server
/// auto-joined us after our knock was accepted, or another of our devices
/// accepted the invite — record the invite acceptance details, as
/// `BaseClient::room_joined` would have done for a client-side join, so that a
/// shared-history room key bundle ([MSC4268]) sent by the inviter can still be
/// accepted.
///
/// The inviter is taken from the previously-stored invite member event when
/// there is one, but a knock that is accepted can transition straight from
/// knocked to joined within one sync (the invite is coalesced away), so we
/// fall back to the `unsigned.prev_sender` of the join event itself (a Synapse
/// extension) when its `prev_content` shows the replaced membership was an
/// invite.
///
/// Also marks a `MEMBERSHIP` notable update for the room, so that the
/// transition is observable and an already-received key bundle gets another
/// chance to be accepted.
///
/// `previous_state` must be the room state from BEFORE this sync response was
/// applied, and `new_state` the state it is transitioning to.
///
/// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
pub async fn record_invite_acceptance_for_server_initiated_join(
    context: &mut Context,
    olm_machine: Option<&OlmMachine>,
    room: &Room,
    previous_state: RoomState,
    new_state: RoomState,
    raw_state_events: &[RawStateEventWithKeys<AnySyncStateEvent>],
) {
    if new_state != RoomState::Joined || previous_state == RoomState::Joined {
        return;
    }

    let Some(olm_machine) = olm_machine else {
        return;
    };

    let user_id = olm_machine.user_id();

    // Source 1: the stored member event for us is (still) the invite.
    let mut inviter: Option<OwnedUserId> = match room.get_member(user_id).await {
        Ok(Some(member)) if *member.event().membership() == MembershipState::Invite => {
            Some(member.event().sender().to_owned())
        }
        Ok(_) => None,
        Err(error) => {
            warn!(room_id = ?room.room_id(), "Failed to fetch our own member event: {error}");
            None
        }
    };

    // Source 2: our join event in this very response replaced an invite —
    // take the inviter from its `unsigned.prev_sender`.
    if inviter.is_none() {
        inviter = raw_state_events.iter().rev().find_map(|event| {
            if event.event_type != StateEventType::RoomMember
                || event.state_key.as_str() != user_id.as_str()
            {
                return None;
            }

            #[derive(Deserialize)]
            struct UnsignedWithPrevSender {
                prev_sender: Option<OwnedUserId>,
                prev_content: Option<RoomMemberEventContent>,
            }

            let content: RoomMemberEventContent = event.raw.get_field("content").ok()??;
            if content.membership != MembershipState::Join {
                return None;
            }

            let unsigned: UnsignedWithPrevSender = event.raw.get_field("unsigned").ok()??;
            if unsigned.prev_content?.membership != MembershipState::Invite {
                return None;
            }

            unsigned.prev_sender
        });
    }

    let Some(inviter) = inviter else {
        return;
    };

    if inviter == user_id {
        return;
    }

    if let Err(error) =
        olm_machine.store().store_room_pending_key_bundle(room.room_id(), &inviter).await
    {
        warn!(room_id = ?room.room_id(), "Failed to record the acceptance of an invite: {error}");
        return;
    }

    // Make sure the membership transition is observable: the section-based
    // (sync v2) and pre-marked (simplified sliding sync) join paths don't
    // reliably emit this reason themselves.
    context
        .room_info_notable_updates
        .entry(room.room_id().to_owned())
        .or_default()
        .insert(RoomInfoNotableUpdateReasons::MEMBERSHIP);
}

/// A classical set of data used by some processors in this module.
#[derive(Clone)]
pub struct E2EE<'a> {
    pub olm_machine: Option<&'a OlmMachine>,
    pub decryption_settings: &'a DecryptionSettings,
    pub verification_is_allowed: bool,
}

impl<'a> E2EE<'a> {
    pub fn new(
        olm_machine: Option<&'a OlmMachine>,
        decryption_settings: &'a DecryptionSettings,
        verification_is_allowed: bool,
    ) -> Self {
        Self { olm_machine, decryption_settings, verification_is_allowed }
    }
}
