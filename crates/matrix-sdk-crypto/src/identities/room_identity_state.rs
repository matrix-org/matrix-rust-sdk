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

use std::{collections::HashMap, ops::Deref};

use matrix_sdk_common::BoxFuture;
use ruma::{
    OwnedUserId, UserId,
    events::{
        SyncStateEvent,
        room::member::{MembershipState, SyncRoomMemberEvent},
    },
};

use super::UserIdentity;
use crate::store::types::IdentityUpdates;

/// Something that can answer questions about the membership of a room and the
/// identities of users.
///
/// This is implemented by `matrix_sdk::Room` and is a trait here so we can
/// supply a mock when needed.
pub trait RoomIdentityProvider: core::fmt::Debug {
    /// Is the user with the supplied ID a member of this room?
    fn is_member<'a>(&'a self, user_id: &'a UserId) -> BoxFuture<'a, bool>;

    /// Return a list of the [`UserIdentity`] of all members of this room
    fn member_identities(&self) -> BoxFuture<'_, Vec<UserIdentity>>;

    /// Return the [`UserIdentity`] of the user with the supplied ID (even if
    /// they are not a member of this room) or None if this user does not
    /// exist.
    fn user_identity<'a>(&'a self, user_id: &'a UserId) -> BoxFuture<'a, Option<UserIdentity>>;

    /// Return the [`IdentityState`] of the supplied user identity.
    /// Normally only overridden in tests.
    fn state_of(&self, user_identity: &UserIdentity) -> IdentityState {
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
        let known_states = KnownStates::from_identities(room.member_identities().await, &room);
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
        sync_room_member_event: Box<SyncRoomMemberEvent>,
    ) -> Vec<IdentityStatusChange> {
        // Ignore redacted events - memberships should come through as new events, not
        // redactions.
        if let SyncStateEvent::Original(event) = sync_room_member_event.deref() {
            // Ignore invalid user IDs
            let user_id: Result<&UserId, _> = event.state_key.as_str().try_into();
            if let Ok(user_id) = user_id {
                // Ignore non-existent users, and changes to our own identity
                if let Some(user_identity @ UserIdentity::Other(_)) =
                    self.room.user_identity(user_id).await
                {
                    // Don't notify on membership changes of verified or pinned identities
                    if matches!(
                        self.room.state_of(&user_identity),
                        IdentityState::Verified | IdentityState::Pinned
                    ) {
                        return vec![];
                    }

                    match event.content.membership {
                        MembershipState::Join | MembershipState::Invite => {
                            // They are joining the room - check whether we need to display a
                            // warning to the user
                            if let Some(update) = self.update_user_state(user_id, &user_identity) {
                                return vec![update];
                            }
                        }
                        MembershipState::Leave | MembershipState::Ban => {
                            // They are leaving the room - treat that as if they are becoming
                            // Pinned, which means the UI will remove any banner it was displaying
                            // for them.

                            if let Some(update) =
                                self.update_user_state_to(user_id, IdentityState::Pinned)
                            {
                                return vec![update];
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
            self.update_user_state_to(user_id, self.room.state_of(user_identity))
        } else {
            // Ignore updates to our own identity
            None
        }
    }

    /// Updates our internal state for this user to the supplied `new_state`. If
    /// the state changed it returns the change information we will surface to
    /// the UI.
    fn update_user_state_to(
        &mut self,
        user_id: &UserId,
        new_state: IdentityState,
    ) -> Option<IdentityStatusChange> {
        let old_state = self.known_states.get(user_id);

        if old_state == new_state {
            return None;
        }

        Some(self.set_state(user_id, new_state))
    }

    fn set_state(&mut self, user_id: &UserId, new_state: IdentityState) -> IdentityStatusChange {
        // Remember the new state of the user
        self.known_states.set(user_id, &new_state);

        // And return the update
        IdentityStatusChange { user_id: user_id.to_owned(), changed_to: new_state }
    }
}

/// A change in the status of the identity of a member of the room. Returned by
/// [`RoomIdentityState::process_change`] to indicate that something significant
/// changed in this room and we should either show or hide a warning.
///
/// Examples of "significant" changes:
/// - pinned->unpinned
/// - verification violation->verified
///
/// Examples of "insignificant" changes:
/// - pinned->verified
/// - verified->pinned
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct IdentityStatusChange {
    /// The user ID of the user whose identity status changed
    pub user_id: OwnedUserId,

    /// The new state of the identity of the user
    pub changed_to: IdentityState,
}

/// The state of an identity - verified, pinned etc.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
    // `Box` the `SyncRoomMemberEvent` to reduce the size of this variant.
    SyncRoomMemberEvent(Box<SyncRoomMemberEvent>),
}

/// What we know about the states of users in this room.
/// Only stores users who _not_ in the Pinned stated.
#[derive(Debug)]
struct KnownStates {
    known_states: HashMap<OwnedUserId, IdentityState>,
}

impl KnownStates {
    fn from_identities(
        member_identities: impl IntoIterator<Item = UserIdentity>,
        room: &dyn RoomIdentityProvider,
    ) -> Self {
        let mut known_states = HashMap::new();
        for user_identity in member_identities {
            let state = room.state_of(&user_identity);
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
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use matrix_sdk_common::BoxFuture;
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{
        OwnedUserId, UserId, device_id, events::room::member::MembershipState, owned_user_id,
        user_id,
    };

    use super::{IdentityState, RoomIdentityChange, RoomIdentityProvider, RoomIdentityState};
    use crate::{
        IdentityStatusChange, OtherUserIdentity, OtherUserIdentityData, OwnUserIdentityData,
        UserIdentity,
        identities::user::testing::own_identity_wrapped,
        store::{Store, types::IdentityUpdates},
    };

    #[async_test]
    async fn test_unpinning_a_pinned_identity_in_the_room_notifies() {
        // Given someone in the room is pinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identity(user_id).await, IdentityState::Pinned);
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When their identity changes to unpinned
        let updates =
            identity_change(&mut room, user_id, IdentityState::PinViolation, false, false).await;
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
    async fn test_verifying_a_pinned_identity_in_the_room_notifies() {
        // Given someone in the room is pinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identity(user_id).await, IdentityState::Pinned);
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When their identity changes to verified
        let updates =
            identity_change(&mut room, user_id, IdentityState::Verified, false, false).await;
        let update = state.process_change(updates).await;

        // Then we emit an update
        assert_eq!(
            update,
            vec![IdentityStatusChange {
                user_id: user_id.to_owned(),
                changed_to: IdentityState::Verified
            }]
        );
    }

    #[async_test]
    async fn test_pinning_an_unpinned_identity_in_the_room_notifies() {
        // Given someone in the room is unpinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identity(user_id).await, IdentityState::PinViolation);
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When their identity changes to pinned
        let updates =
            identity_change(&mut room, user_id, IdentityState::Pinned, false, false).await;
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
    async fn test_unpinned_identity_becoming_verification_violating_in_the_room_notifies() {
        // Given someone in the room is unpinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identity(user_id).await, IdentityState::PinViolation);
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When their identity changes to verification violation
        let updates =
            identity_change(&mut room, user_id, IdentityState::VerificationViolation, false, false)
                .await;
        let update = state.process_change(updates).await;

        // Then we emit an update saying they became verification violating
        assert_eq!(
            update,
            vec![IdentityStatusChange {
                user_id: user_id.to_owned(),
                changed_to: IdentityState::VerificationViolation
            }]
        );
    }

    #[async_test]
    async fn test_unpinning_an_identity_not_in_the_room_does_nothing() {
        // Given an empty room
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When a new unpinned user identity appears but they are not in the room
        let updates =
            identity_change(&mut room, user_id, IdentityState::PinViolation, true, false).await;
        let update = state.process_change(updates).await;

        // Then we emit no update
        assert_eq!(update, vec![]);
    }

    #[async_test]
    async fn test_pinning_an_identity_not_in_the_room_does_nothing() {
        // Given an empty room
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When a new pinned user appears but is not in the room
        let updates = identity_change(&mut room, user_id, IdentityState::Pinned, true, false).await;
        let update = state.process_change(updates).await;

        // Then we emit no update
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_pinning_an_already_pinned_identity_in_the_room_does_nothing() {
        // Given someone in the room is pinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identity(user_id).await, IdentityState::Pinned);
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When we are told they are pinned
        let updates =
            identity_change(&mut room, user_id, IdentityState::Pinned, false, false).await;
        let update = state.process_change(updates).await;

        // Then we emit no update
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_unpinning_an_already_unpinned_identity_in_the_room_does_nothing() {
        // Given someone in the room is unpinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identity(user_id).await, IdentityState::PinViolation);
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When we are told they are unpinned
        let updates =
            identity_change(&mut room, user_id, IdentityState::PinViolation, false, false).await;
        let update = state.process_change(updates).await;

        // Then we emit no update
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_a_pinned_identity_joining_the_room_does_nothing() {
        // Given an empty room and we know of a user who is pinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.non_member(other_user_identity(user_id).await, IdentityState::Pinned);
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When the pinned user joins the room
        let updates = room_change(user_id, MembershipState::Join);
        let update = state.process_change(updates).await;

        // Then we emit no update because they are pinned
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_a_verified_identity_joining_the_room_does_nothing() {
        // Given an empty room and we know of a user who is verified
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.non_member(other_user_identity(user_id).await, IdentityState::Verified);
        let mut state = RoomIdentityState::new(room).await;

        // When the verified user joins the room
        let updates = room_change(user_id, MembershipState::Join);
        let update = state.process_change(updates).await;

        // Then we emit no update because they are verified
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_an_unpinned_identity_joining_the_room_notifies() {
        // Given an empty room and we know of a user who is unpinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.non_member(other_user_identity(user_id).await, IdentityState::PinViolation);
        let mut state = RoomIdentityState::new(room.clone()).await;

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
        room.non_member(other_user_identity(user_id).await, IdentityState::Pinned);
        let mut state = RoomIdentityState::new(room.clone()).await;

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
        room.non_member(other_user_identity(user_id).await, IdentityState::PinViolation);
        let mut state = RoomIdentityState::new(room.clone()).await;

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
    async fn test_a_verification_violating_identity_invited_to_the_room_notifies() {
        // Given an empty room and we know of a user who is unpinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.non_member(other_user_identity(user_id).await, IdentityState::VerificationViolation);
        let mut state = RoomIdentityState::new(room).await;

        // When the user is invited to the room
        let updates = room_change(user_id, MembershipState::Invite);
        let update = state.process_change(updates).await;

        // Then we emit an update saying they became verification violation
        assert_eq!(
            update,
            vec![IdentityStatusChange {
                user_id: user_id.to_owned(),
                changed_to: IdentityState::VerificationViolation
            }]
        );
    }

    #[async_test]
    async fn test_own_identity_becoming_unpinned_is_ignored() {
        // Given I am pinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(own_user_identity(user_id).await, IdentityState::Pinned);
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When I become unpinned
        let updates =
            identity_change(&mut room, user_id, IdentityState::PinViolation, false, true).await;
        let update = state.process_change(updates).await;

        // Then we do nothing because own identities are ignored
        assert_eq!(update, vec![]);
    }

    #[async_test]
    async fn test_own_identity_becoming_pinned_is_ignored() {
        // Given I am unpinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(own_user_identity(user_id).await, IdentityState::PinViolation);
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When I become unpinned
        let updates = identity_change(&mut room, user_id, IdentityState::Pinned, false, true).await;
        let update = state.process_change(updates).await;

        // Then we do nothing because own identities are ignored
        assert_eq!(update, vec![]);
    }

    #[async_test]
    async fn test_own_pinned_identity_joining_room_is_ignored() {
        // Given an empty room and we know of a user who is pinned
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.non_member(own_user_identity(user_id).await, IdentityState::Pinned);
        let mut state = RoomIdentityState::new(room.clone()).await;

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
        room.non_member(own_user_identity(user_id).await, IdentityState::PinViolation);
        let mut state = RoomIdentityState::new(room.clone()).await;

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
        room.member(other_user_identity(user_id).await, IdentityState::Pinned);
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When the pinned user leaves the room
        let updates = room_change(user_id, MembershipState::Leave);
        let update = state.process_change(updates).await;

        // Then we emit no update because they are pinned
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_a_verified_identity_leaving_the_room_does_nothing() {
        // Given a verified user is in the room
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identity(user_id).await, IdentityState::Verified);
        let mut state = RoomIdentityState::new(room).await;

        // When the user leaves the room
        let updates = room_change(user_id, MembershipState::Leave);
        let update = state.process_change(updates).await;

        // Then we emit no update because they are verified
        assert_eq!(update, []);
    }

    #[async_test]
    async fn test_an_unpinned_identity_leaving_the_room_notifies() {
        // Given an unpinned user is in the room
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identity(user_id).await, IdentityState::PinViolation);
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When the unpinned user leaves the room
        let updates = room_change(user_id, MembershipState::Leave);
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
    async fn test_a_verification_violating_identity_leaving_the_room_notifies() {
        // Given an unpinned user is in the room
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identity(user_id).await, IdentityState::VerificationViolation);
        let mut state = RoomIdentityState::new(room).await;

        // When the user leaves the room
        let updates = room_change(user_id, MembershipState::Leave);
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
    async fn test_a_pinned_identity_being_banned_does_nothing() {
        // Given a pinned user is in the room
        let user_id = user_id!("@u:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identity(user_id).await, IdentityState::Pinned);
        let mut state = RoomIdentityState::new(room.clone()).await;

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
        room.member(other_user_identity(user_id).await, IdentityState::PinViolation);
        let mut state = RoomIdentityState::new(room.clone()).await;

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
        room.member(other_user_identity(user1).await, IdentityState::Pinned);
        room.member(other_user_identity(user2).await, IdentityState::PinViolation);
        room.member(other_user_identity(user3).await, IdentityState::Pinned);
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When they all change state simultaneously
        let updates = identity_changes(
            &mut room,
            &[
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
            ],
        )
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
        room.member(other_user_identity(user_id).await, IdentityState::Pinned);
        let mut state = RoomIdentityState::new(room.clone()).await;

        // When they change state multiple times
        let update1 = state
            .process_change(
                identity_change(&mut room, user_id, IdentityState::PinViolation, false, false)
                    .await,
            )
            .await;
        let update2 = state
            .process_change(
                identity_change(&mut room, user_id, IdentityState::PinViolation, false, false)
                    .await,
            )
            .await;
        let update3 = state
            .process_change(
                identity_change(&mut room, user_id, IdentityState::Pinned, false, false).await,
            )
            .await;
        let update4 = state
            .process_change(
                identity_change(&mut room, user_id, IdentityState::PinViolation, false, false)
                    .await,
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
        room.member(other_user_identity(user1).await, IdentityState::Pinned);
        room.member(other_user_identity(user2).await, IdentityState::Pinned);
        let state = RoomIdentityState::new(room).await;
        assert!(state.current_state().is_empty());
    }

    #[async_test]
    async fn test_current_state_contains_all_nonpinned_users() {
        // Given some people are unpinned
        let user1 = user_id!("@u1:s.co");
        let user2 = user_id!("@u2:s.co");
        let user3 = user_id!("@u3:s.co");
        let user4 = user_id!("@u4:s.co");
        let user5 = user_id!("@u5:s.co");
        let user6 = user_id!("@u6:s.co");
        let mut room = FakeRoom::new();
        room.member(other_user_identity(user1).await, IdentityState::Pinned);
        room.member(other_user_identity(user2).await, IdentityState::PinViolation);
        room.member(other_user_identity(user3).await, IdentityState::Pinned);
        room.member(other_user_identity(user4).await, IdentityState::PinViolation);
        room.member(other_user_identity(user5).await, IdentityState::Verified);
        room.member(other_user_identity(user6).await, IdentityState::VerificationViolation);
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
                },
                IdentityStatusChange {
                    user_id: owned_user_id!("@u5:s.co"),
                    changed_to: IdentityState::Verified
                },
                IdentityStatusChange {
                    user_id: owned_user_id!("@u6:s.co"),
                    changed_to: IdentityState::VerificationViolation
                }
            ]
        );
    }

    #[derive(Debug)]
    struct Membership {
        is_member: bool,
        user_identity: UserIdentity,
        identity_state: IdentityState,
    }

    #[derive(Clone, Debug)]
    struct FakeRoom {
        users: Arc<Mutex<HashMap<OwnedUserId, Membership>>>,
    }

    impl FakeRoom {
        fn new() -> Self {
            Self { users: Default::default() }
        }

        fn member(&mut self, user_identity: UserIdentity, identity_state: IdentityState) {
            self.users.lock().unwrap().insert(
                user_identity.user_id().to_owned(),
                Membership { is_member: true, user_identity, identity_state },
            );
        }

        fn non_member(&mut self, user_identity: UserIdentity, identity_state: IdentityState) {
            self.users.lock().unwrap().insert(
                user_identity.user_id().to_owned(),
                Membership { is_member: false, user_identity, identity_state },
            );
        }

        fn update_state(&self, user_id: &UserId, changed_to: &IdentityState) {
            self.users
                .lock()
                .unwrap()
                .entry(user_id.to_owned())
                .and_modify(|m| m.identity_state = changed_to.clone());
        }
    }

    impl RoomIdentityProvider for FakeRoom {
        fn is_member<'a>(&'a self, user_id: &'a UserId) -> BoxFuture<'a, bool> {
            Box::pin(async {
                self.users.lock().unwrap().get(user_id).map(|m| m.is_member).unwrap_or(false)
            })
        }

        fn member_identities(&self) -> BoxFuture<'_, Vec<UserIdentity>> {
            Box::pin(async {
                self.users
                    .lock()
                    .unwrap()
                    .values()
                    .filter_map(|m| if m.is_member { Some(m.user_identity.clone()) } else { None })
                    .collect()
            })
        }

        fn user_identity<'a>(&'a self, user_id: &'a UserId) -> BoxFuture<'a, Option<UserIdentity>> {
            Box::pin(async {
                self.users.lock().unwrap().get(user_id).map(|m| m.user_identity.clone())
            })
        }

        fn state_of(&self, user_identity: &UserIdentity) -> IdentityState {
            self.users
                .lock()
                .unwrap()
                .get(user_identity.user_id())
                .map(|m| m.identity_state.clone())
                .unwrap_or(IdentityState::Pinned)
        }
    }

    fn room_change(user_id: &UserId, new_state: MembershipState) -> RoomIdentityChange {
        let event = EventFactory::new()
            .sender(user_id!("@admin:b.c"))
            .member(user_id)
            .membership(new_state)
            .into();
        RoomIdentityChange::SyncRoomMemberEvent(Box::new(event))
    }

    async fn identity_change(
        room: &mut FakeRoom,
        user_id: &UserId,
        changed_to: IdentityState,
        new: bool,
        own: bool,
    ) -> RoomIdentityChange {
        identity_changes(
            room,
            &[IdentityChangeSpec { user_id: user_id.to_owned(), changed_to, new, own }],
        )
        .await
    }

    struct IdentityChangeSpec {
        user_id: OwnedUserId,
        changed_to: IdentityState,
        new: bool,
        own: bool,
    }

    async fn identity_changes(
        room: &mut FakeRoom,
        changes: &[IdentityChangeSpec],
    ) -> RoomIdentityChange {
        let mut updates = IdentityUpdates::default();

        for change in changes {
            let user_identity = if change.own {
                own_user_identity(&change.user_id).await
            } else {
                other_user_identity(&change.user_id).await
            };

            room.update_state(user_identity.user_id(), &change.changed_to);
            if change.new {
                updates.new.insert(user_identity.user_id().to_owned(), user_identity);
            } else {
                updates.changed.insert(user_identity.user_id().to_owned(), user_identity);
            }
        }
        RoomIdentityChange::IdentityUpdates(updates)
    }

    /// Create an other `UserIdentity` for use in tests
    async fn other_user_identity(user_id: &UserId) -> UserIdentity {
        use std::sync::Arc;

        use ruma::owned_device_id;
        use tokio::sync::Mutex;

        use crate::{
            Account,
            olm::PrivateCrossSigningIdentity,
            store::{CryptoStoreWrapper, MemoryStore},
            verification::VerificationMachine,
        };

        let device_id = owned_device_id!("DEV123");
        let account = Account::with_device_id(user_id, &device_id);

        let private_identity =
            Arc::new(Mutex::new(PrivateCrossSigningIdentity::for_account(&account)));

        let other_user_identity_data =
            OtherUserIdentityData::from_private(&*private_identity.lock().await).await;

        UserIdentity::Other(OtherUserIdentity {
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
        })
    }

    /// Create an own `UserIdentity` for use in tests
    async fn own_user_identity(user_id: &UserId) -> UserIdentity {
        use std::sync::Arc;

        use ruma::owned_device_id;
        use tokio::sync::Mutex;

        use crate::{
            Account,
            olm::PrivateCrossSigningIdentity,
            store::{CryptoStoreWrapper, MemoryStore},
            verification::VerificationMachine,
        };

        let device_id = owned_device_id!("DEV123");
        let account = Account::with_device_id(user_id, &device_id);

        let private_identity =
            Arc::new(Mutex::new(PrivateCrossSigningIdentity::for_account(&account)));

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

        UserIdentity::Own(own_identity_wrapped(
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
        ))
    }
}
