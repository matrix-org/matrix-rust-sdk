// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use std::collections::HashMap;

use async_trait::async_trait;
use ruma::{
    events::{
        room::member::{MembershipState, SyncRoomMemberEvent},
        SyncStateEvent,
    },
    OwnedUserId, UserId,
};

use super::UserIdentity;
use crate::store::IdentityUpdates;

/// Something that can answer questions about the membership of a room and the
/// identities of users.
///
/// This is implemented by `matrix_sdk::Room` and is a trait here so we can
/// supply a mock when needed.
#[async_trait]
pub trait RoomIdentityProvider: core::fmt::Debug {
    /// Is the user with the supplied ID a member of this room?
    async fn is_member(&self, user_id: &UserId) -> bool;

    /// Return a list of the [`UserIdentity`] of all members of this room
    async fn member_identities(&self) -> Vec<UserIdentity>;

    /// Return the [`UserIdentity`] of the user with the supplied ID (even if
    /// they are not a member of this room) or None if this user does not
    /// exist.
    async fn user_identity(&self, user_id: &UserId) -> Option<UserIdentity>;
}

/// The state of the identities in a given room - whether they are:
///
/// * in pin violation (the identity changed after we accepted their identity),
/// * verified (we manually did the emoji dance),
/// * previously verified (we did the emoji dance and then their identity
///   changed),
/// * otherwise, they are pinned.
#[derive(Debug)]
pub struct RoomIdentityState<R: RoomIdentityProvider> {
    room: R,
    known_states: KnownStates,
}

impl<R: RoomIdentityProvider> RoomIdentityState<R> {
    /// Create a new RoomIdentityState using the provided room to check whether
    /// users are members.
    pub async fn new(room: R) -> Self {
        let known_states = KnownStates::from_identities(room.member_identities().await);
        Self { room, known_states }
    }

    /// Provide the current state of the room: a list of all the non-pinned
    /// identities and their status.
    pub fn current_state(&self) -> Vec<IdentityStatusChange> {
        self.known_states
            .known_states
            .iter()
            .map(|(user_id, state)| IdentityStatusChange {
                user_id: user_id.clone(),
                changed_to: state.clone(),
            })
            .collect()
    }

    /// Deal with an incoming event - either someone's identity changed, or some
    /// changes happened to a room's membership.
    ///
    /// Returns the changes (if any) to the list of valid/invalid identities in
    /// the room.
    pub async fn process_change(&mut self, item: RoomIdentityChange) -> Vec<IdentityStatusChange> {
        match item {
            RoomIdentityChange::IdentityUpdates(identity_updates) => {
                self.process_identity_changes(identity_updates).await
            }
            RoomIdentityChange::SyncRoomMemberEvent(sync_room_member_event) => {
                self.process_membership_change(sync_room_member_event).await
            }
        }
    }

    async fn process_identity_changes(
        &mut self,
        identity_updates: IdentityUpdates,
    ) -> Vec<IdentityStatusChange> {
        let mut ret = vec![];

        for user_identity in identity_updates.new.values().chain(identity_updates.changed.values())
        {
            // Ignore updates to our own identity
            let user_id = user_identity.user_id();
            if self.room.is_member(user_id).await {
                let update = self.update_user_state(user_id, user_identity);
                if let Some(identity_status_change) = update {
                    ret.push(identity_status_change);
                }
            }
        }

        ret
    }

    async fn process_membership_change(
        &mut self,
        sync_room_member_event: SyncRoomMemberEvent,
    ) -> Vec<IdentityStatusChange> {
        // Ignore redacted events - memberships should come through as new events, not
        // redactions.
        if let SyncStateEvent::Original(event) = sync_room_member_event {
            // Ignore invalid user IDs
            let user_id: Result<&UserId, _> = event.state_key.as_str().try_into();
            if let Ok(user_id) = user_id {
                // Ignore non-existent users
                if let Some(user_identity) = self.room.user_identity(user_id).await {
                    match event.content.membership {
                        MembershipState::Join | MembershipState::Invite => {
                            if let Some(update) = self.update_user_state(user_id, &user_identity) {
                                return vec![update];
                            }
                        }
                        MembershipState::Leave | MembershipState::Ban => {
                            let leaving_state = state_of(&user_identity);
                            if leaving_state == IdentityState::PinViolation {
                                // If a user with bad state leaves the room, set them to Pinned,
                                // which effectively removes them
                                return vec![self.set_state(user_id, IdentityState::Pinned)];
                            }
                        }
                        MembershipState::Knock => {
                            // No need to do anything when someone is knocking
                        }
                        _ => {}
                    }
                }
            }
        }

        // We didn't find a relevant update, so return an empty list
        vec![]
    }

    fn update_user_state(
        &mut self,
        user_id: &UserId,
        user_identity: &UserIdentity,
    ) -> Option<IdentityStatusChange> {
        if let UserIdentity::Other(_) = &user_identity {
            // If the user's state has changed
            let new_state = state_of(user_identity);
            let old_state = self.known_states.get(user_id);
            if new_state != old_state {
                Some(self.set_state(user_identity.user_id(), new_state))
            } else {
                // Nothing changed
                None
            }
        } else {
            // Ignore updates to our own identity
            None
        }
    }

    fn set_state(&mut self, user_id: &UserId, new_state: IdentityState) -> IdentityStatusChange {
        // Remember the new state of the user
        self.known_states.set(user_id, &new_state);

        // And return the update
        IdentityStatusChange { user_id: user_id.to_owned(), changed_to: new_state }
    }
}

fn state_of(user_identity: &UserIdentity) -> IdentityState {
    if user_identity.is_verified() {
        IdentityState::Verified
    } else if user_identity.has_verification_violation() {
        IdentityState::VerificationViolation
    } else if let UserIdentity::Other(u) = user_identity {
        if u.identity_needs_user_approval() {
            IdentityState::PinViolation
        } else {
            IdentityState::Pinned
        }
    } else {
        IdentityState::Pinned
    }
}

/// A change in the status of the identity of a member of the room. Returned by
/// [`RoomIdentityState::process_change`] to indicate that something changed in
/// this room and we should either show or hide a warning.
#[derive(Clone, Debug, PartialEq)]
pub struct IdentityStatusChange {
    /// The user ID of the user whose identity status changed
    pub user_id: OwnedUserId,

    /// The new state of the identity of the user
    pub changed_to: IdentityState,
}

/// The state of an identity - verified, pinned etc.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum IdentityState {
    /// The user is verified with us
    Verified,

    /// Either this is the first identity we have seen for this user, or the
    /// user has acknowledged a change of identity explicitly e.g. by
    /// clicking OK on a notification.
    Pinned,

    /// The user's identity has changed since it was pinned. The user should be
    /// notified about this and given the opportunity to acknowledge the
    /// change, which will make the new identity pinned.
    /// When the user acknowledges the change, the app should call
    /// [`crate::OtherUserIdentity::pin_current_master_key`].
    PinViolation,

    /// The user's identity has changed, and before that it was verified. This
    /// is a serious problem. The user can either verify again to make this
    /// identity verified, or withdraw verification
    /// [`UserIdentity::withdraw_verification`] to make it pinned.
    VerificationViolation,
}

/// The type of update that can be received by
/// [`RoomIdentityState::process_change`] - either a change of someone's
/// identity, or a change of room membership.
#[derive(Debug)]
pub enum RoomIdentityChange {
    /// Someone's identity changed
    IdentityUpdates(IdentityUpdates),

    /// Someone joined or left a room
    SyncRoomMemberEvent(SyncRoomMemberEvent),
}

/// What we know about the states of users in this room.
/// Only stores users who _not_ in the Pinned stated.
#[derive(Debug)]
struct KnownStates {
    known_states: HashMap<OwnedUserId, IdentityState>,
}

impl KnownStates {
    fn from_identities(member_identities: impl IntoIterator<Item = UserIdentity>) -> Self {
        let mut known_states = HashMap::new();
        for user_identity in member_identities {
            let state = state_of(&user_identity);
            if state != IdentityState::Pinned {
                known_states.insert(user_identity.user_id().to_owned(), state);
            }
        }
        Self { known_states }
    }

    /// Return the known state of the supplied user, or IdentityState::Pinned if
    /// we don't know.
    fn get(&self, user_id: &UserId) -> IdentityState {
        self.known_states.get(user_id).cloned().unwrap_or(IdentityState::Pinned)
    }

    /// Set the supplied user's state to the state given. If identity_state is
    /// IdentityState::Pinned, forget this user.
    fn set(&mut self, user_id: &UserId, identity_state: &IdentityState) {
        if let IdentityState::Pinned = identity_state {
            self.known_states.remove(user_id);
        } else {
            self.known_states.insert(user_id.to_owned(), identity_state.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use matrix_sdk_test::async_test;
    use ruma::{
        device_id,
        events::{
            room::member::{
                MembershipState, RoomMemberEventContent, RoomMemberUnsigned, SyncRoomMemberEvent,
            },
            OriginalSyncStateEvent,
        },
        owned_event_id, owned_user_id, user_id, MilliSecondsSinceUnixEpoch, OwnedUserId, UInt,
        UserId,
    };
    use tokio::sync::Mutex;

    use super::{IdentityState, RoomIdentityChange, RoomIdentityProvider, RoomIdentityState};
    use crate::{
        identities::user::testing::own_identity_wrapped,
        olm::PrivateCrossSigningIdentity,
        store::{IdentityUpdates, Store},
        Account, IdentityStatusChange, OtherUserIdentity, OtherUserIdentityData, OwnUserIdentity,
        OwnUserIdentityData, UserIdentity,
    };

    #[async_test]
    async fn test_unpinning_a_pinned_identity_in_the_room_notifies() {
        // Given someone in the room is pinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identities(user_id, IdentityState::Pinned).await);
        let mut state = RoomIdentityState::new(room).await;

        // When their identity changes to unpinned
        let updates = identity_change(user_id, IdentityState::PinViolation, false, false).await;
        let update = state.process_change(updates).await;

        // Then we emit an update saying they became unpinned
        assert_eq!(
            update,
            vec![IdentityStatusChange {
                user_id: user_id.to_owned(),
                changed_to: IdentityState::PinViolation
            }]
        );
    }

    #[async_test]
    async fn test_pinning_an_unpinned_identity_in_the_room_notifies() {
        // Given someone in the room is unpinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identities(user_id, IdentityState::PinViolation).await);
        let mut state = RoomIdentityState::new(room).await;

        // When their identity changes to pinned
        let updates = identity_change(user_id, IdentityState::Pinned, false, false).await;
        let update = state.process_change(updates).await;

        // Then we emit an update saying they became pinned
        assert_eq!(
            update,
            vec![IdentityStatusChange {
                user_id: user_id.to_owned(),
                changed_to: IdentityState::Pinned
            }]
        );
    }

    #[async_test]
    async fn test_unpinning_an_identity_not_in_the_room_does_nothing() {
        // Given an empty room
        let user_id = user_id!("@u:s.co");
        let room = FakeRoom::new();
        let mut state = RoomIdentityState::new(room).await;

        // When a new unpinned user identity appears but they are not in the room
        let updates = identity_change(user_id, IdentityState::PinViolation, true, false).await;
        let update = state.process_change(updates).await;

        // Then we emit no update
        assert_eq!(update, vec![]);
    }

    #[async_test]
    async fn test_pinning_an_identity_not_in_the_room_does_nothing() {
        // Given an empty room
        let user_id = user_id!("@u:s.co");
        let room = FakeRoom::new();
        let mut state = RoomIdentityState::new(room).await;

        // When a new pinned user appears but is not in the room
        let updates = identity_change(user_id, IdentityState::Pinned, true, false).await;
        let update = state.process_change(updates).await;

        // Then we emit no update
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_pinning_an_already_pinned_identity_in_the_room_does_nothing() {
        // Given someone in the room is pinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identities(user_id, IdentityState::Pinned).await);
        let mut state = RoomIdentityState::new(room).await;

        // When we are told they are pinned
        let updates = identity_change(user_id, IdentityState::Pinned, false, false).await;
        let update = state.process_change(updates).await;

        // Then we emit no update
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_unpinning_an_already_unpinned_identity_in_the_room_does_nothing() {
        // Given someone in the room is unpinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identities(user_id, IdentityState::PinViolation).await);
        let mut state = RoomIdentityState::new(room).await;

        // When we are told they are unpinned
        let updates = identity_change(user_id, IdentityState::PinViolation, false, false).await;
        let update = state.process_change(updates).await;

        // Then we emit no update
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_a_pinned_identity_joining_the_room_does_nothing() {
        // Given an empty room and we know of a user who is pinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.non_member(other_user_identities(user_id, IdentityState::Pinned).await);
        let mut state = RoomIdentityState::new(room).await;

        // When the pinned user joins the room
        let updates = room_change(user_id, MembershipState::Join);
        let update = state.process_change(updates).await;

        // Then we emit no update because they are pinned
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_an_unpinned_identity_joining_the_room_notifies() {
        // Given an empty room and we know of a user who is unpinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.non_member(other_user_identities(user_id, IdentityState::PinViolation).await);
        let mut state = RoomIdentityState::new(room).await;

        // When the unpinned user joins the room
        let updates = room_change(user_id, MembershipState::Join);
        let update = state.process_change(updates).await;

        // Then we emit an update saying they became unpinned
        assert_eq!(
            update,
            vec![IdentityStatusChange {
                user_id: user_id.to_owned(),
                changed_to: IdentityState::PinViolation
            }]
        );
    }

    #[async_test]
    async fn test_a_pinned_identity_invited_to_the_room_does_nothing() {
        // Given an empty room and we know of a user who is pinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.non_member(other_user_identities(user_id, IdentityState::Pinned).await);
        let mut state = RoomIdentityState::new(room).await;

        // When the pinned user is invited to the room
        let updates = room_change(user_id, MembershipState::Invite);
        let update = state.process_change(updates).await;

        // Then we emit no update because they are pinned
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_an_unpinned_identity_invited_to_the_room_notifies() {
        // Given an empty room and we know of a user who is unpinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.non_member(other_user_identities(user_id, IdentityState::PinViolation).await);
        let mut state = RoomIdentityState::new(room).await;

        // When the unpinned user is invited to the room
        let updates = room_change(user_id, MembershipState::Invite);
        let update = state.process_change(updates).await;

        // Then we emit an update saying they became unpinned
        assert_eq!(
            update,
            vec![IdentityStatusChange {
                user_id: user_id.to_owned(),
                changed_to: IdentityState::PinViolation
            }]
        );
    }

    #[async_test]
    async fn test_own_identity_becoming_unpinned_is_ignored() {
        // Given I am pinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(own_user_identities(user_id, IdentityState::Pinned).await);
        let mut state = RoomIdentityState::new(room).await;

        // When I become unpinned
        let updates = identity_change(user_id, IdentityState::PinViolation, false, true).await;
        let update = state.process_change(updates).await;

        // Then we do nothing because own identities are ignored
        assert_eq!(update, vec![]);
    }

    #[async_test]
    async fn test_own_identity_becoming_pinned_is_ignored() {
        // Given I am unpinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(own_user_identities(user_id, IdentityState::PinViolation).await);
        let mut state = RoomIdentityState::new(room).await;

        // When I become unpinned
        let updates = identity_change(user_id, IdentityState::Pinned, false, true).await;
        let update = state.process_change(updates).await;

        // Then we do nothing because own identities are ignored
        assert_eq!(update, vec![]);
    }

    #[async_test]
    async fn test_own_pinned_identity_joining_room_is_ignored() {
        // Given an empty room and we know of a user who is pinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.non_member(own_user_identities(user_id, IdentityState::Pinned).await);
        let mut state = RoomIdentityState::new(room).await;

        // When the pinned user joins the room
        let updates = room_change(user_id, MembershipState::Join);
        let update = state.process_change(updates).await;

        // Then we emit no update because this is our own identity
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_own_unpinned_identity_joining_room_is_ignored() {
        // Given an empty room and we know of a user who is unpinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.non_member(own_user_identities(user_id, IdentityState::PinViolation).await);
        let mut state = RoomIdentityState::new(room).await;

        // When the unpinned user joins the room
        let updates = room_change(user_id, MembershipState::Join);
        let update = state.process_change(updates).await;

        // Then we emit no update because this is our own identity
        assert_eq!(update, vec![]);
    }

    #[async_test]
    async fn test_a_pinned_identity_leaving_the_room_does_nothing() {
        // Given a pinned user is in the room
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identities(user_id, IdentityState::Pinned).await);
        let mut state = RoomIdentityState::new(room).await;

        // When the pinned user leaves the room
        let updates = room_change(user_id, MembershipState::Leave);
        let update = state.process_change(updates).await;

        // Then we emit no update because they are pinned
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_an_unpinned_identity_leaving_the_room_notifies() {
        // Given an unpinned user is in the room
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identities(user_id, IdentityState::PinViolation).await);
        let mut state = RoomIdentityState::new(room).await;

        // When the unpinned user leaves the room
        let updates = room_change(user_id, MembershipState::Leave);
        let update = state.process_change(updates).await;

        // Then we emit an update saying they became unpinned
        assert_eq!(
            update,
            vec![IdentityStatusChange {
                user_id: user_id.to_owned(),
                changed_to: IdentityState::Pinned
            }]
        );
    }

    #[async_test]
    async fn test_a_pinned_identity_being_banned_does_nothing() {
        // Given a pinned user is in the room
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identities(user_id, IdentityState::Pinned).await);
        let mut state = RoomIdentityState::new(room).await;

        // When the pinned user is banned
        let updates = room_change(user_id, MembershipState::Ban);
        let update = state.process_change(updates).await;

        // Then we emit no update because they are pinned
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_an_unpinned_identity_being_banned_notifies() {
        // Given an unpinned user is in the room
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identities(user_id, IdentityState::PinViolation).await);
        let mut state = RoomIdentityState::new(room).await;

        // When the unpinned user is banned
        let updates = room_change(user_id, MembershipState::Ban);
        let update = state.process_change(updates).await;

        // Then we emit an update saying they became unpinned
        assert_eq!(
            update,
            vec![IdentityStatusChange {
                user_id: user_id.to_owned(),
                changed_to: IdentityState::Pinned
            }]
        );
    }

    #[async_test]
    async fn test_multiple_simultaneous_identity_updates_are_all_notified() {
        // Given several people in the room with different states
        let user1 = user_id!("@u1:s.co");
        let user2 = user_id!("@u2:s.co");
        let user3 = user_id!("@u3:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identities(user1, IdentityState::Pinned).await);
        room.member(other_user_identities(user2, IdentityState::PinViolation).await);
        room.member(other_user_identities(user3, IdentityState::Pinned).await);
        let mut state = RoomIdentityState::new(room).await;

        // When they all change state simultaneously
        let updates = identity_changes(&[
            IdentityChangeSpec {
                user_id: user1.to_owned(),
                changed_to: IdentityState::PinViolation,
                new: false,
                own: false,
            },
            IdentityChangeSpec {
                user_id: user2.to_owned(),
                changed_to: IdentityState::Pinned,
                new: false,
                own: false,
            },
            IdentityChangeSpec {
                user_id: user3.to_owned(),
                changed_to: IdentityState::PinViolation,
                new: false,
                own: false,
            },
        ])
        .await;
        let update = state.process_change(updates).await;

        // Then we emit updates for each of them
        assert_eq!(
            update,
            vec![
                IdentityStatusChange {
                    user_id: user1.to_owned(),
                    changed_to: IdentityState::PinViolation
                },
                IdentityStatusChange {
                    user_id: user2.to_owned(),
                    changed_to: IdentityState::Pinned
                },
                IdentityStatusChange {
                    user_id: user3.to_owned(),
                    changed_to: IdentityState::PinViolation
                }
            ]
        );
    }

    #[async_test]
    async fn test_multiple_changes_are_notified() {
        // Given someone in the room is pinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identities(user_id, IdentityState::Pinned).await);
        let mut state = RoomIdentityState::new(room).await;

        // When they change state multiple times
        let update1 = state
            .process_change(
                identity_change(user_id, IdentityState::PinViolation, false, false).await,
            )
            .await;
        let update2 = state
            .process_change(
                identity_change(user_id, IdentityState::PinViolation, false, false).await,
            )
            .await;
        let update3 = state
            .process_change(identity_change(user_id, IdentityState::Pinned, false, false).await)
            .await;
        let update4 = state
            .process_change(
                identity_change(user_id, IdentityState::PinViolation, false, false).await,
            )
            .await;

        // Then we emit updates each time
        assert_eq!(
            update1,
            vec![IdentityStatusChange {
                user_id: user_id.to_owned(),
                changed_to: IdentityState::PinViolation
            }]
        );
        // (Except update2 where nothing changed)
        assert_eq!(update2, vec![]);
        assert_eq!(
            update3,
            vec![IdentityStatusChange {
                user_id: user_id.to_owned(),
                changed_to: IdentityState::Pinned
            }]
        );
        assert_eq!(
            update4,
            vec![IdentityStatusChange {
                user_id: user_id.to_owned(),
                changed_to: IdentityState::PinViolation
            }]
        );
    }

    #[async_test]
    async fn test_current_state_of_all_pinned_room_is_empty() {
        // Given everyone in the room is pinned
        let user1 = user_id!("@u1:s.co");
        let user2 = user_id!("@u2:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identities(user1, IdentityState::Pinned).await);
        room.member(other_user_identities(user2, IdentityState::Pinned).await);
        let state = RoomIdentityState::new(room).await;
        assert!(state.current_state().is_empty());
    }

    #[async_test]
    async fn test_current_state_contains_all_unpinned_users() {
        // Given some people are unpinned
        let user1 = user_id!("@u1:s.co");
        let user2 = user_id!("@u2:s.co");
        let user3 = user_id!("@u3:s.co");
        let user4 = user_id!("@u4:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identities(user1, IdentityState::Pinned).await);
        room.member(other_user_identities(user2, IdentityState::PinViolation).await);
        room.member(other_user_identities(user3, IdentityState::Pinned).await);
        room.member(other_user_identities(user4, IdentityState::PinViolation).await);
        let mut state = RoomIdentityState::new(room).await.current_state();
        state.sort_by_key(|change| change.user_id.to_owned());
        assert_eq!(
            state,
            vec![
                IdentityStatusChange {
                    user_id: owned_user_id!("@u2:s.co"),
                    changed_to: IdentityState::PinViolation
                },
                IdentityStatusChange {
                    user_id: owned_user_id!("@u4:s.co"),
                    changed_to: IdentityState::PinViolation
                }
            ]
        );
    }

    #[derive(Debug)]
    struct FakeRoom {
        members: Vec<UserIdentity>,
        non_members: Vec<UserIdentity>,
    }

    impl FakeRoom {
        fn new() -> Self {
            Self { members: Default::default(), non_members: Default::default() }
        }

        fn member(&mut self, user_identity: UserIdentity) {
            self.members.push(user_identity);
        }

        fn non_member(&mut self, user_identity: UserIdentity) {
            self.non_members.push(user_identity);
        }
    }

    #[async_trait]
    impl RoomIdentityProvider for FakeRoom {
        async fn is_member(&self, user_id: &UserId) -> bool {
            self.members.iter().any(|u| u.user_id() == user_id)
        }

        async fn member_identities(&self) -> Vec<UserIdentity> {
            self.members.clone()
        }

        async fn user_identity(&self, user_id: &UserId) -> Option<UserIdentity> {
            self.non_members
                .iter()
                .chain(self.members.iter())
                .find(|u| u.user_id() == user_id)
                .cloned()
        }
    }

    fn room_change(user_id: &UserId, new_state: MembershipState) -> RoomIdentityChange {
        let event = SyncRoomMemberEvent::Original(OriginalSyncStateEvent {
            content: RoomMemberEventContent::new(new_state),
            event_id: owned_event_id!("$1"),
            sender: owned_user_id!("@admin:b.c"),
            origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new(2123).unwrap()),
            unsigned: RoomMemberUnsigned::new(),
            state_key: user_id.to_owned(),
        });
        RoomIdentityChange::SyncRoomMemberEvent(event)
    }

    async fn identity_change(
        user_id: &UserId,
        changed_to: IdentityState,
        new: bool,
        own: bool,
    ) -> RoomIdentityChange {
        identity_changes(&[IdentityChangeSpec {
            user_id: user_id.to_owned(),
            changed_to,
            new,
            own,
        }])
        .await
    }

    struct IdentityChangeSpec {
        user_id: OwnedUserId,
        changed_to: IdentityState,
        new: bool,
        own: bool,
    }

    async fn identity_changes(changes: &[IdentityChangeSpec]) -> RoomIdentityChange {
        let mut updates = IdentityUpdates::default();

        for change in changes {
            let user_identities = if change.own {
                let user_identity =
                    own_user_identity(&change.user_id, change.changed_to.clone()).await;
                UserIdentity::Own(user_identity)
            } else {
                let user_identity =
                    other_user_identity(&change.user_id, change.changed_to.clone()).await;
                UserIdentity::Other(user_identity)
            };

            if change.new {
                updates.new.insert(user_identities.user_id().to_owned(), user_identities);
            } else {
                updates.changed.insert(user_identities.user_id().to_owned(), user_identities);
            }
        }
        RoomIdentityChange::IdentityUpdates(updates)
    }

    /// Create an other `UserIdentity`
    async fn other_user_identities(
        user_id: &UserId,
        identity_state: IdentityState,
    ) -> UserIdentity {
        UserIdentity::Other(other_user_identity(user_id, identity_state).await)
    }

    /// Create an other `UserIdentity` for use in tests
    async fn other_user_identity(
        user_id: &UserId,
        identity_state: IdentityState,
    ) -> OtherUserIdentity {
        use std::sync::Arc;

        use ruma::owned_device_id;
        use tokio::sync::Mutex;

        use crate::{
            olm::PrivateCrossSigningIdentity,
            store::{CryptoStoreWrapper, MemoryStore},
            verification::VerificationMachine,
            Account,
        };

        let device_id = owned_device_id!("DEV123");
        let account = Account::with_device_id(user_id, &device_id);

        let private_identity =
            Arc::new(Mutex::new(PrivateCrossSigningIdentity::with_account(&account).await.0));

        let other_user_identity_data =
            OtherUserIdentityData::from_private(&*private_identity.lock().await).await;

        let mut user_identity = OtherUserIdentity {
            inner: other_user_identity_data,
            own_identity: None,
            verification_machine: VerificationMachine::new(
                account.clone(),
                Arc::new(Mutex::new(PrivateCrossSigningIdentity::new(
                    account.user_id().to_owned(),
                ))),
                Arc::new(CryptoStoreWrapper::new(
                    account.user_id(),
                    account.device_id(),
                    MemoryStore::new(),
                )),
            ),
        };

        match identity_state {
            IdentityState::Verified => {
                // TODO
                assert!(user_identity.is_verified());
                assert!(!user_identity.was_previously_verified());
                assert!(!user_identity.has_verification_violation());
                assert!(!user_identity.identity_needs_user_approval());
            }
            IdentityState::Pinned => {
                // Pinned is the default state
                assert!(!user_identity.is_verified());
                assert!(!user_identity.was_previously_verified());
                assert!(!user_identity.has_verification_violation());
                assert!(!user_identity.identity_needs_user_approval());
            }
            IdentityState::PinViolation => {
                change_master_key(&mut user_identity, &account).await;
                assert!(!user_identity.is_verified());
                assert!(!user_identity.was_previously_verified());
                assert!(!user_identity.has_verification_violation());
                assert!(user_identity.identity_needs_user_approval());
            }
            IdentityState::VerificationViolation => {
                // TODO
                assert!(!user_identity.is_verified());
                assert!(user_identity.was_previously_verified());
                assert!(!user_identity.has_verification_violation());
                assert!(user_identity.identity_needs_user_approval());
            }
        }

        user_identity
    }

    /// Create an own `UserIdentity`
    async fn own_user_identities(user_id: &UserId, identity_state: IdentityState) -> UserIdentity {
        UserIdentity::Own(own_user_identity(user_id, identity_state).await)
    }

    /// Create an own `UserIdentity` for use in tests
    async fn own_user_identity(user_id: &UserId, identity_state: IdentityState) -> OwnUserIdentity {
        use std::sync::Arc;

        use ruma::owned_device_id;
        use tokio::sync::Mutex;

        use crate::{
            olm::PrivateCrossSigningIdentity,
            store::{CryptoStoreWrapper, MemoryStore},
            verification::VerificationMachine,
            Account,
        };

        let device_id = owned_device_id!("DEV123");
        let account = Account::with_device_id(user_id, &device_id);

        let private_identity =
            Arc::new(Mutex::new(PrivateCrossSigningIdentity::with_account(&account).await.0));

        let own_user_identity_data =
            OwnUserIdentityData::from_private(&*private_identity.lock().await).await;

        let cross_signing_identity = PrivateCrossSigningIdentity::new(account.user_id().to_owned());
        let verification_machine = VerificationMachine::new(
            account.clone(),
            Arc::new(Mutex::new(cross_signing_identity.clone())),
            Arc::new(CryptoStoreWrapper::new(
                account.user_id(),
                account.device_id(),
                MemoryStore::new(),
            )),
        );

        let mut user_identity = own_identity_wrapped(
            own_user_identity_data,
            verification_machine.clone(),
            Store::new(
                account.static_data().clone(),
                Arc::new(Mutex::new(cross_signing_identity)),
                Arc::new(CryptoStoreWrapper::new(
                    user_id!("@u:s.co"),
                    device_id!("DEV7"),
                    MemoryStore::new(),
                )),
                verification_machine,
            ),
        );

        match identity_state {
            IdentityState::Verified => {
                // TODO
                assert!(user_identity.is_verified());
                assert!(!user_identity.was_previously_verified());
                assert!(!user_identity.has_verification_violation());
            }
            IdentityState::Pinned => {
                // Pinned is the default state
                assert!(!user_identity.is_verified());
                assert!(!user_identity.was_previously_verified());
                assert!(!user_identity.has_verification_violation());
            }
            IdentityState::PinViolation => {
                change_own_master_key(&mut user_identity, &account).await;
                assert!(!user_identity.is_verified());
                assert!(!user_identity.was_previously_verified());
                assert!(!user_identity.has_verification_violation());
            }
            IdentityState::VerificationViolation => {
                // TODO
                assert!(!user_identity.is_verified());
                assert!(user_identity.was_previously_verified());
                assert!(!user_identity.has_verification_violation());
            }
        }

        user_identity
    }

    async fn change_master_key(user_identity: &mut OtherUserIdentity, account: &Account) {
        // Create a new master key and self signing key
        let private_identity =
            Arc::new(Mutex::new(PrivateCrossSigningIdentity::with_account(account).await.0));
        let data = OtherUserIdentityData::from_private(&*private_identity.lock().await).await;

        // And set them on the existing identity
        user_identity
            .update(data.master_key().clone(), data.self_signing_key().clone(), None)
            .unwrap();
    }

    async fn change_own_master_key(user_identity: &mut OwnUserIdentity, account: &Account) {
        // Create a new master key and self signing key
        let private_identity =
            Arc::new(Mutex::new(PrivateCrossSigningIdentity::with_account(account).await.0));
        let data = OwnUserIdentityData::from_private(&*private_identity.lock().await).await;

        // And set them on the existing identity
        user_identity
            .update(
                data.master_key().clone(),
                data.self_signing_key().clone(),
                data.user_signing_key().clone(),
            )
            .unwrap();
    }
}
