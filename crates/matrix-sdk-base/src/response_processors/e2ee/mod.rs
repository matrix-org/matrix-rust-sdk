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

use std::time::Duration;

use matrix_sdk_crypto::{DecryptionSettings, OlmMachine};
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedUserId, UserId,
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

/// How long after a join event was created we still consider it a candidate
/// for a server-initiated join, when the room was unknown to this client
/// before the sync response (e.g. the first sync after the server auto-joined
/// us). This bounds the work done on an initial sync of an existing account.
const UNKNOWN_ROOM_JOIN_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Membership details of one of our own `m.room.member` events in a sync
/// response.
struct OwnMemberEvent {
    sender: OwnedUserId,
    membership: MembershipState,
    prev_membership: Option<MembershipState>,
    origin_server_ts: Option<MilliSecondsSinceUnixEpoch>,
}

impl OwnMemberEvent {
    fn parse(event: &RawStateEventWithKeys<AnySyncStateEvent>, user_id: &UserId) -> Option<Self> {
        if event.event_type != StateEventType::RoomMember
            || event.state_key.as_str() != user_id.as_str()
        {
            return None;
        }

        #[derive(Deserialize)]
        struct Unsigned {
            prev_content: Option<RoomMemberEventContent>,
        }

        let content: RoomMemberEventContent = event.raw.get_field("content").ok()??;
        let sender: OwnedUserId = event.raw.get_field("sender").ok()??;
        let origin_server_ts = event.raw.get_field("origin_server_ts").ok().flatten();
        let prev_membership = event
            .raw
            .get_field::<Unsigned>("unsigned")
            .ok()
            .flatten()
            .and_then(|unsigned| unsigned.prev_content)
            .map(|prev| prev.membership);

        Some(Self { sender, membership: content.membership, prev_membership, origin_server_ts })
    }
}

/// If a sync response transitions the current user's membership in `room` to
/// joined WITHOUT this client having performed the join (a *server-initiated
/// join*: the server auto-accepted an invite, or auto-joined us after our
/// knock was accepted, or another of our devices accepted the invite), treat
/// it as an invite acceptance for the purpose of [MSC4268] history sharing.
///
/// The inviter is determined from signed room events only:
///
/// 1. the stored invite member event, if this client saw the invite;
/// 2. otherwise, our own `invite` member event in this very sync response.
///
/// If the inviter is found, the invite acceptance details are recorded right
/// away. In every case the room gets the
/// [`RoomInfoNotableUpdateReasons::SERVER_INITIATED_JOIN`] notable update, so
/// that the key bundle receiver can import an already-received bundle, or look
/// up the inviter in the room history when it could not be found locally
/// (e.g. with sliding sync, where the invite is not part of the response).
///
/// `previous_state` is the room state BEFORE this sync response was applied,
/// or `None` if the room was unknown to this client.
///
/// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
pub async fn record_invite_acceptance_for_server_initiated_join(
    context: &mut Context,
    olm_machine: Option<&OlmMachine>,
    room: &Room,
    previous_state: Option<RoomState>,
    new_state: RoomState,
    raw_state_events: &[RawStateEventWithKeys<AnySyncStateEvent>],
) {
    if new_state != RoomState::Joined || previous_state == Some(RoomState::Joined) {
        return;
    }

    let Some(olm_machine) = olm_machine else {
        return;
    };

    let user_id = olm_machine.user_id();

    let own_events: Vec<OwnMemberEvent> =
        raw_state_events.iter().filter_map(|event| OwnMemberEvent::parse(event, user_id)).collect();

    let own_join = own_events.iter().rev().find(|event| event.membership == MembershipState::Join);

    // Does this join plausibly replace an invite?
    let replaces_invite = match previous_state {
        // We saw the invite (or our knock, which can only become a join by
        // being invited when the room requires knocking).
        Some(RoomState::Invited | RoomState::Knocked) => true,

        // We were not in the room, or didn't know it at all: rely on the
        // join event itself, which tells us which membership it replaced.
        Some(RoomState::Left | RoomState::Banned) | None => own_join.is_some_and(|join| {
            let replaced_invite = join.prev_membership == Some(MembershipState::Invite)
                || own_events.iter().any(|event| event.membership == MembershipState::Invite);

            let recent_enough = previous_state.is_some()
                || join
                    .origin_server_ts
                    .and_then(|ts| ts.to_system_time())
                    .and_then(|ts| ts.elapsed().ok())
                    .is_some_and(|age| age < UNKNOWN_ROOM_JOIN_MAX_AGE);

            replaced_invite && recent_enough
        }),

        Some(RoomState::Joined) => false,
    };

    if !replaces_invite {
        return;
    }

    // Source 1: the stored member event for us is (still) the invite.
    let mut inviter: Option<OwnedUserId> = None;
    if previous_state == Some(RoomState::Invited) {
        match room.get_member(user_id).await {
            Ok(Some(member)) if *member.event().membership() == MembershipState::Invite => {
                inviter = Some(member.event().sender().to_owned());
            }
            Ok(_) => {}
            Err(error) => {
                warn!(room_id = ?room.room_id(), "Failed to fetch our own member event: {error}");
            }
        }
    }

    // Source 2: our own invite event is part of this sync response.
    if inviter.is_none() {
        inviter = own_events
            .iter()
            .rev()
            .find(|event| event.membership == MembershipState::Invite)
            .map(|event| event.sender.clone());
    }

    if let Some(inviter) = inviter.filter(|inviter| inviter != user_id)
        && let Err(error) =
            olm_machine.store().store_room_pending_key_bundle(room.room_id(), &inviter).await
    {
        warn!(room_id = ?room.room_id(), "Failed to record the acceptance of an invite: {error}");
    }

    // Make the server-initiated join observable, so that the key bundle
    // receiver imports a stored bundle, or looks up the inviter in the room
    // history if we could not determine it here.
    context.room_info_notable_updates.entry(room.room_id().to_owned()).or_default().insert(
        RoomInfoNotableUpdateReasons::MEMBERSHIP
            | RoomInfoNotableUpdateReasons::SERVER_INITIATED_JOIN,
    );
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
