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

use ruma::{
    RoomId,
    events::{
        SyncStateEvent,
        room::member::{MembershipState, RoomMemberEventContent},
    },
};

use super::Context;

/// Decide whether the profile must be created, updated or deleted based on the
/// [`RoomMemberEventContent`].
pub fn upsert_or_delete(
    context: &mut Context,
    room_id: &RoomId,
    event: &SyncStateEvent<RoomMemberEventContent>,
) {
    // Senders can fake the profile easily so we keep track of profiles that the
    // member set themselves to avoid having confusing profile changes when a
    // member gets kicked/banned.
    if event.state_key() == event.sender() {
        context
            .state_changes
            .profiles
            .entry(room_id.to_owned())
            .or_default()
            .insert(event.sender().to_owned(), event.into());
    }

    if *event.membership() == MembershipState::Invite {
        // Remove any profile previously stored for the invited user.
        //
        // A room member could have joined the room and left it later; in that case, the
        // server may return a dummy, empty profile along the `leave` event. We
        // don't want to reuse that empty profile when the member has been
        // re-invited, so we remove it from the database.
        context
            .state_changes
            .profiles_to_delete
            .entry(room_id.to_owned())
            .or_default()
            .push(event.state_key().clone());
    }
}
