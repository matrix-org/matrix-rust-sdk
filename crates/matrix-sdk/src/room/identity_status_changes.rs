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

//! Facility to track changes to the identity of members of rooms.
#![cfg(feature = "e2e-encryption")]

use std::collections::BTreeMap;

use async_stream::stream;
use futures_core::Stream;
use futures_util::{StreamExt, stream_select};
use matrix_sdk_base::crypto::{
    IdentityState, IdentityStatusChange, RoomIdentityChange, RoomIdentityState,
};
use ruma::{OwnedUserId, UserId, events::room::member::SyncRoomMemberEvent};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::Room;
use crate::{
    Client, Error, Result,
    encryption::identities::{IdentityUpdates, UserIdentity},
    event_handler::EventHandlerDropGuard,
};

/// Support for creating a stream of batches of [`IdentityStatusChange`].
///
/// Internally, this subscribes to all identity changes, and to room events that
/// change the membership, and provides a stream of all changes to the identity
/// status of all room members.
///
/// This struct does not represent the actual stream, but the state that is used
/// to produce the values of the stream.
///
/// It does provide a method to create the stream:
/// [`IdentityStatusChanges::create_stream`].
#[derive(Debug)]
pub struct IdentityStatusChanges {
    /// Who is in the room and who is in identity violation at this moment
    room_identity_state: RoomIdentityState<Room>,

    /// Dropped when we are dropped, and unregisters the event handler we
    /// registered to listen for room events
    _drop_guard: EventHandlerDropGuard,
}

impl IdentityStatusChanges {
    /// Create a new stream of significant changes to the identity status of
    /// members of a room.
    ///
    /// The "status" of an identity changes when our level of trust in it
    /// changes.
    ///
    /// A "significant" change means a warning should either be added or removed
    /// (e.g. the user changed from pinned to unpinned (show a warning) or
    /// from verification violation to pinned (remove a warning). An
    /// insignificant change would be from pinned to verified - no warning
    /// is needed in this case.
    ///
    /// For example, if an identity is "pinned" i.e. not manually verified, but
    /// known, and it becomes a "unpinned" i.e. unknown, because the
    /// encryption keys are different and the user has not acknowledged
    /// this, then this constitutes a status change. Also, if an identity is
    /// "unpinned" and becomes "pinned", this is also a status change.
    ///
    /// The supplied stream is intended to provide enough information for a
    /// client to display a list of room members whose identities have
    /// changed, and allow the user to acknowledge this or act upon it.
    ///
    /// The first item in the stream provides the current state of the room:
    /// each member of the room who is not in "pinned" or "verified" state will
    /// be included (except the current user).
    ///
    /// Note: when an unpinned user leaves a room, an update is generated
    /// stating that they have become pinned, even though they may not
    /// necessarily have become pinned, but we don't care any more because they
    /// left the room.
    pub async fn create_stream(
        room: Room,
    ) -> Result<impl Stream<Item = Vec<IdentityStatusChange>>> {
        let identity_updates = wrap_identity_updates(&room.client).await?;
        let (drop_guard, room_member_events) = wrap_room_member_events(&room);
        let mut unprocessed_stream = combine_streams(identity_updates, room_member_events);
        let own_user_id = room.client.user_id().ok_or(Error::InsufficientData)?.to_owned();

        let state = IdentityStatusChanges {
            room_identity_state: RoomIdentityState::new(room).await,
            _drop_guard: drop_guard,
        };

        Ok(stream!({
            // Force enclosing stream to take ownership of state, so that
            // _drop_guard does not get dropped.
            let mut state = state;

            let mut current_state =
                filter_for_initial_update(state.room_identity_state.current_state(), &own_user_id);

            if !current_state.is_empty() {
                current_state.sort();
                yield current_state;
            }

            while let Some(item) = unprocessed_stream.next().await {
                let mut update = filter_non_self(
                    state.room_identity_state.process_change(item).await,
                    &own_user_id,
                );
                if !update.is_empty() {
                    update.sort();
                    yield update;
                }
            }
        }))
    }
}

fn filter_for_initial_update(
    mut input: Vec<IdentityStatusChange>,
    own_user_id: &UserId,
) -> Vec<IdentityStatusChange> {
    // We are never interested in changes to our own identity, and also for initial
    // updates, we are only interested in "bad" states where we need to
    // notify the user, so we can remove Verified states (Pinned states are
    // already missing, because Pinned is considered the default).
    input.retain(|change| {
        change.user_id != own_user_id && change.changed_to != IdentityState::Verified
    });

    input
}

fn filter_non_self(
    mut input: Vec<IdentityStatusChange>,
    own_user_id: &UserId,
) -> Vec<IdentityStatusChange> {
    // We are never interested in changes to our own identity
    input.retain(|change| change.user_id != own_user_id);
    input
}

fn combine_streams(
    identity_updates: impl Stream<Item = RoomIdentityChange> + Unpin,
    room_member_events: impl Stream<Item = RoomIdentityChange> + Unpin,
) -> impl Stream<Item = RoomIdentityChange> {
    stream_select!(identity_updates, room_member_events)
}

async fn wrap_identity_updates(
    client: &Client,
) -> Result<impl Stream<Item = RoomIdentityChange> + use<>> {
    Ok(client
        .encryption()
        .user_identities_stream()
        .await?
        .map(|item| RoomIdentityChange::IdentityUpdates(to_base_updates(item))))
}

fn to_base_updates(
    input: IdentityUpdates,
) -> matrix_sdk_base::crypto::store::types::IdentityUpdates {
    matrix_sdk_base::crypto::store::types::IdentityUpdates {
        new: to_base_identities(input.new),
        changed: to_base_identities(input.changed),
        unchanged: Default::default(),
    }
}

fn to_base_identities(
    input: BTreeMap<OwnedUserId, UserIdentity>,
) -> BTreeMap<OwnedUserId, matrix_sdk_base::crypto::UserIdentity> {
    input.into_iter().map(|(k, v)| (k, v.underlying_identity())).collect()
}

fn wrap_room_member_events(
    room: &Room,
) -> (EventHandlerDropGuard, impl Stream<Item = RoomIdentityChange> + use<>) {
    let own_user_id = room.own_user_id().to_owned();
    let room_id = room.room_id();
    let (sender, receiver) = mpsc::channel(16);
    let handle =
        room.client.add_room_event_handler(room_id, move |event: SyncRoomMemberEvent| async move {
            if *event.state_key() == own_user_id {
                return;
            }
            let _: Result<_, _> =
                sender.send(RoomIdentityChange::SyncRoomMemberEvent(Box::new(event))).await;
        });
    let drop_guard = room.client.event_handler_drop_guard(handle);
    (drop_guard, ReceiverStream::new(receiver))
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::time::Duration;

    use futures_util::{FutureExt as _, StreamExt as _, pin_mut};
    use matrix_sdk_base::crypto::IdentityState;
    use matrix_sdk_test::{async_test, test_json::keys_query_sets::IdentityChangeDataSet};
    use test_setup::TestSetup;

    use crate::assert_next_with_timeout;

    #[async_test]
    async fn test_when_user_becomes_unpinned_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_bob().await;

        // And Bob's identity is pinned
        t.pin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob becomes unpinned
        t.unpin_bob().await;

        // Then we were notified about it
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_user_becomes_verification_violation_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_bob().await;

        // And Bob's identity is verified
        t.verify_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob's identity changes
        t.unpin_bob().await;

        // Then we were notified about a verification violation
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::VerificationViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_user_becomes_pinned_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_bob().await;

        // And Bob's identity is unpinned
        t.unpin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob becomes pinned
        t.pin_bob().await;

        // Then we were notified about the initial state of the room
        let change1 = assert_next_with_timeout!(stream);
        assert_eq!(change1[0].user_id, t.bob_user_id());
        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change1.len(), 1);

        // And the change when Bob became pinned
        let change2 = assert_next_with_timeout!(stream);
        assert_eq!(change2[0].user_id, t.bob_user_id());
        assert_eq!(change2[0].changed_to, IdentityState::Pinned);
        assert_eq!(change2.len(), 1);
    }

    #[async_test]
    async fn test_when_user_becomes_verified_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob becomes verified
        t.verify_bob().await;

        // Then we are notified about Bob being verified
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::Verified);
        assert_eq!(change.len(), 1);

        // (And then unpinned, so we have something to come through the stream)
        t.unpin_bob().await;

        // Then we are notified about the unpinning part
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::VerificationViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_an_unpinned_user_becomes_verified_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_bob().await;

        // And Bob's identity is unpinned
        t.unpin_bob_with(IdentityChangeDataSet::key_query_with_identity_a()).await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob becomes verified
        t.verify_bob().await;

        // Then we were notified about the initial state of the room
        let change1 = assert_next_with_timeout!(stream);
        assert_eq!(change1[0].user_id, t.bob_user_id());
        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change1.len(), 1);

        // And the change when Bob became verified
        let change2 = assert_next_with_timeout!(stream);
        assert_eq!(change2[0].user_id, t.bob_user_id());
        assert_eq!(change2[0].changed_to, IdentityState::Verified);
        assert_eq!(change2.len(), 1);
    }

    #[async_test]
    async fn test_when_user_in_verification_violation_becomes_verified_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_bob().await;

        // And Bob is in verification violation
        t.verify_bob_with(
            IdentityChangeDataSet::key_query_with_identity_b(),
            IdentityChangeDataSet::master_signing_keys_b(),
            IdentityChangeDataSet::self_signing_keys_b(),
        )
        .await;
        t.unpin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob becomes verified
        t.verify_bob().await;

        // Then we were notified about the initial state of the room
        let change1 = assert_next_with_timeout!(stream);
        assert_eq!(change1[0].user_id, t.bob_user_id());
        assert_eq!(change1[0].changed_to, IdentityState::VerificationViolation);
        assert_eq!(change1.len(), 1);

        // And the change when Bob became verified
        let change2 = assert_next_with_timeout!(stream);
        assert_eq!(change2[0].user_id, t.bob_user_id());
        assert_eq!(change2[0].changed_to, IdentityState::Verified);
        assert_eq!(change2.len(), 1);
    }

    #[async_test]
    async fn test_when_an_unpinned_user_joins_we_report_it() {
        // Given a room containing just us
        let mut t = TestSetup::new_just_me_room().await;

        // And Bob's identity is unpinned
        t.unpin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob joins the room
        t.bob_joins().await;

        // Then we were notified about it
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_an_verification_violating_user_joins_we_report_it() {
        // Given a room containing just us
        let mut t = TestSetup::new_just_me_room().await;

        // And Bob's identity is in verification violation
        t.verify_bob().await;
        t.unpin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob joins the room
        t.bob_joins().await;

        // Then we were notified about it
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::VerificationViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_a_verified_user_joins_we_dont_report_it() {
        // Given a room containing just us
        let mut t = TestSetup::new_just_me_room().await;

        // And Bob's identity is verified
        t.verify_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob joins the room
        t.bob_joins().await;

        // (Then becomes unpinned so we have something to report)
        t.unpin_bob().await;

        //// Then we were only notified about the unpin
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::VerificationViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_a_pinned_user_joins_we_do_not_report() {
        // Given a room containing just us
        let mut t = TestSetup::new_just_me_room().await;

        // And Bob's identity is unpinned
        t.pin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob joins the room
        t.bob_joins().await;

        // Then there is no notification
        tokio::time::sleep(Duration::from_millis(200)).await;
        let change = stream.next().now_or_never();
        assert!(change.is_none());
    }

    #[async_test]
    async fn test_when_an_unpinned_user_leaves_we_report_it() {
        // Given a room containing us and Bob
        let mut t = TestSetup::new_room_with_other_bob().await;

        // And Bob's identity is unpinned
        t.unpin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // When Bob leaves the room
        t.bob_leaves().await;

        // Then we were notified about the initial state of the room
        let change1 = assert_next_with_timeout!(stream);
        assert_eq!(change1[0].user_id, t.bob_user_id());
        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change1.len(), 1);

        // And we were notified about the change when the user left
        let change2 = assert_next_with_timeout!(stream);
        // Note: the user left the room, but we see that as them "becoming pinned" i.e.
        // "you no longer need to notify about this user".
        assert_eq!(change2[0].user_id, t.bob_user_id());
        assert_eq!(change2[0].changed_to, IdentityState::Pinned);
        assert_eq!(change2.len(), 1);
    }

    #[async_test]
    async fn test_multiple_identity_changes_are_reported() {
        // Given a room containing just us
        let mut t = TestSetup::new_just_me_room().await;

        // And Bob's identity is unpinned
        t.unpin_bob().await;

        // And we are listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // NOTE: below we pull the changes out of the subscription after each action.
        // This makes sure that the identity changes and membership changes are properly
        // ordered. If we pull them out later, the identity changes get shifted forward
        // because they rely on less-complex async stuff under the hood. Calling
        // next_change ends up winding the async machinery sufficiently that the
        // membership change and any subsequent events have fully completed.

        // When Bob joins the room ...
        t.bob_joins().await;
        let change1 = assert_next_with_timeout!(stream);

        // ... becomes pinned ...
        t.pin_bob().await;
        let change2 = assert_next_with_timeout!(stream);

        // ... leaves and joins again (ignored since they stay pinned) ...
        t.bob_leaves().await;
        t.bob_joins().await;

        // ... becomes unpinned ...
        t.unpin_bob().await;
        let change3 = assert_next_with_timeout!(stream);

        // ... and leaves.
        t.bob_leaves().await;
        let change4 = assert_next_with_timeout!(stream);

        assert_eq!(change1[0].user_id, t.bob_user_id());
        assert_eq!(change2[0].user_id, t.bob_user_id());
        assert_eq!(change3[0].user_id, t.bob_user_id());
        assert_eq!(change4[0].user_id, t.bob_user_id());

        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change2[0].changed_to, IdentityState::Pinned);
        assert_eq!(change3[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change4[0].changed_to, IdentityState::Pinned);

        assert_eq!(change1.len(), 1);
        assert_eq!(change2.len(), 1);
        assert_eq!(change3.len(), 1);
        assert_eq!(change4.len(), 1);
    }

    #[async_test]
    async fn test_when_an_unpinned_user_is_already_present_we_report_it_immediately() {
        // Given a room containing Bob, who is unpinned
        let t = TestSetup::new_room_with_other_bob().await;
        t.unpin_bob().await;

        // When we start listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // Then we were immediately notified about Bob being unpinned
        let change = assert_next_with_timeout!(stream);
        assert_eq!(change[0].user_id, t.bob_user_id());
        assert_eq!(change[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_a_verified_user_is_already_present_we_dont_report_it() {
        // Given a room containing Bob, who is unpinned
        let t = TestSetup::new_room_with_other_bob().await;
        t.verify_bob().await;

        // When we start listening for identity changes
        let stream = t.subscribe_to_identity_status_changes().await;
        pin_mut!(stream);

        // (And we unpin so that something is available in the changes stream)
        t.unpin_bob().await;

        // Then we were only notified about the unpin, not being verified
        let next_change = assert_next_with_timeout!(stream);

        assert_eq!(next_change[0].user_id, t.bob_user_id());
        assert_eq!(next_change[0].changed_to, IdentityState::VerificationViolation);
        assert_eq!(next_change.len(), 1);
    }

    // TODO: I (andyb) haven't figured out how to test room membership changes that
    // affect our own user (they should not be shown). Specifically, I haven't
    // figure out how to get out own user into a non-pinned state.

    mod test_setup {
        use std::time::{SystemTime, UNIX_EPOCH};

        use futures_core::Stream;
        use matrix_sdk_base::{
            RoomState,
            crypto::{
                IdentityStatusChange, OtherUserIdentity,
                testing::simulate_key_query_response_for_verification,
            },
        };
        use matrix_sdk_test::{
            DEFAULT_TEST_ROOM_ID, JoinedRoomBuilder, StateTestEvent, SyncResponseBuilder,
            test_json, test_json::keys_query_sets::IdentityChangeDataSet,
        };
        use ruma::{
            OwnedUserId, TransactionId, UserId,
            api::client::keys::{get_keys, get_keys::v3::Response as KeyQueryResponse},
            events::room::member::MembershipState,
            owned_user_id,
        };
        use serde_json::json;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{header, method, path_regex},
        };

        use crate::{
            Client, Room, encryption::identities::UserIdentity, test_utils::logged_in_client,
        };

        /// Sets up a client and a room and allows changing user identities and
        /// room memberships. Note: most methods e.g. [`TestSetup::bob_user_id`]
        /// are talking about the OTHER user, not our own user. Only
        /// methods starting with `self_` are talking about this user.
        ///
        /// This user is called `@example:localhost` but is rarely used
        /// mentioned.
        ///
        /// The other user is called `@bob:localhost`.
        pub(super) struct TestSetup {
            client: Client,
            bob_user_id: OwnedUserId,
            sync_response_builder: SyncResponseBuilder,
            room: Room,
        }

        impl TestSetup {
            pub(super) async fn new_just_me_room() -> Self {
                let (client, user_id, mut sync_response_builder) = Self::init().await;
                let room = create_just_me_room(&client, &mut sync_response_builder).await;
                Self { client, bob_user_id: user_id, sync_response_builder, room }
            }

            pub(super) async fn new_room_with_other_bob() -> Self {
                let (client, bob_user_id, mut sync_response_builder) = Self::init().await;
                let room = create_room_with_other_member(
                    &mut sync_response_builder,
                    &client,
                    &bob_user_id,
                )
                .await;
                Self { client, bob_user_id, sync_response_builder, room }
            }

            pub(super) fn bob_user_id(&self) -> &UserId {
                &self.bob_user_id
            }

            pub(super) async fn pin_bob(&self) {
                if self.bob_user_identity().await.is_some() {
                    assert!(
                        !self.bob_is_pinned().await,
                        "pin_bob() called when the identity is already pinned!"
                    );

                    // Pin it
                    self.bob_user_identity()
                        .await
                        .expect("User should exist")
                        .pin()
                        .await
                        .expect("Should not fail to pin");
                } else {
                    // There was no existing identity. Set one. It will be pinned by default.
                    self.change_bob_identity(IdentityChangeDataSet::key_query_with_identity_a())
                        .await;
                }

                // Sanity check: they are pinned
                assert!(self.bob_is_pinned().await);
            }

            pub(super) async fn unpin_bob(&self) {
                self.unpin_bob_with(IdentityChangeDataSet::key_query_with_identity_b()).await;
            }

            pub(super) async fn unpin_bob_with(&self, requested: KeyQueryResponse) {
                fn master_key_json(key_query_response: &KeyQueryResponse) -> String {
                    serde_json::to_string(
                        key_query_response
                            .master_keys
                            .first_key_value()
                            .expect("Master key should have a value")
                            .1,
                    )
                    .expect("Should be able to serialise master key")
                }

                let a = IdentityChangeDataSet::key_query_with_identity_a();
                let b = IdentityChangeDataSet::key_query_with_identity_b();
                let requested_master_key = master_key_json(&requested);
                let a_master_key = master_key_json(&a);

                // Change/set their identity pin it, then change it again - this will definitely
                // unpin, even if the first identity we supply is their very first, making them
                // initially pinned.
                if requested_master_key == a_master_key {
                    self.change_bob_identity(b).await;
                    if !self.bob_is_pinned().await {
                        self.pin_bob().await;
                    }
                    self.change_bob_identity(a).await;
                } else {
                    self.change_bob_identity(a).await;
                    if !self.bob_is_pinned().await {
                        self.pin_bob().await;
                    }
                    self.change_bob_identity(b).await;
                }

                // Sanity: they are unpinned
                assert!(!self.bob_is_pinned().await);
            }

            pub(super) async fn verify_bob(&self) {
                self.verify_bob_with(
                    IdentityChangeDataSet::key_query_with_identity_a(),
                    IdentityChangeDataSet::master_signing_keys_a(),
                    IdentityChangeDataSet::self_signing_keys_a(),
                )
                .await;
            }

            pub(super) async fn verify_bob_with(
                &self,
                key_query: KeyQueryResponse,
                master_signing_key: serde_json::Value,
                self_signing_key: serde_json::Value,
            ) {
                // Make sure the requested identity is set
                self.change_bob_identity(key_query).await;

                let my_user_id = self.client.user_id().expect("I should have a user id");
                let my_identity = self
                    .client
                    .encryption()
                    .get_user_identity(my_user_id)
                    .await
                    .expect("Should not fail to get own user identity")
                    .expect("Should have an own user identity")
                    .underlying_identity()
                    .own()
                    .expect("Our own identity should be of type Own");

                // Get the request
                let signature_upload_request = self
                    .bob_crypto_other_identity()
                    .await
                    .verify()
                    .await
                    .expect("Should be able to verify other identity");

                let verification_response = simulate_key_query_response_for_verification(
                    signature_upload_request,
                    my_identity,
                    my_user_id,
                    self.bob_user_id(),
                    master_signing_key,
                    self_signing_key,
                );

                // Receive the response into our client
                self.client
                    .mark_request_as_sent(&TransactionId::new(), &verification_response)
                    .await
                    .unwrap();

                // Sanity: they are verified
                assert!(self.bob_is_verified().await);
            }

            pub(super) async fn bob_joins(&mut self) {
                self.bob_membership_change(MembershipState::Join).await;
            }

            pub(super) async fn bob_leaves(&mut self) {
                self.bob_membership_change(MembershipState::Leave).await;
            }

            pub(super) async fn subscribe_to_identity_status_changes(
                &self,
            ) -> impl Stream<Item = Vec<IdentityStatusChange>> + use<> {
                self.room
                    .subscribe_to_identity_status_changes()
                    .await
                    .expect("Should be able to subscribe")
            }

            async fn init() -> (Client, OwnedUserId, SyncResponseBuilder) {
                let (client, _server) = create_client_and_server().await;

                // Ensure our user has cross-signing keys etc.
                client
                    .olm_machine()
                    .await
                    .as_ref()
                    .expect("We should have an Olm machine")
                    .bootstrap_cross_signing(true)
                    .await
                    .expect("Should be able to bootstrap cross-signing");

                // Note: if you change the user_id, you will need to change lots of hard-coded
                // stuff inside IdentityChangeDataSet
                let bob_user_id = owned_user_id!("@bob:localhost");

                let sync_response_builder = SyncResponseBuilder::default();

                (client, bob_user_id, sync_response_builder)
            }

            async fn change_bob_identity(
                &self,
                key_query_response: get_keys::v3::Response,
            ) -> OtherUserIdentity {
                self.client
                    .mark_request_as_sent(&TransactionId::new(), &key_query_response)
                    .await
                    .expect("Should not fail to send identity changes");

                self.bob_crypto_other_identity().await
            }

            async fn bob_membership_change(&mut self, new_state: MembershipState) {
                let sync_response = self
                    .sync_response_builder
                    .add_joined_room(JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID).add_state_event(
                        StateTestEvent::Custom(sync_response_member(
                            &self.bob_user_id,
                            new_state.clone(),
                        )),
                    ))
                    .build_sync_response();
                self.room.client.process_sync(sync_response).await.unwrap();

                // Make sure the membership stuck as expected
                let m = self
                    .room
                    .get_member_no_sync(&self.bob_user_id)
                    .await
                    .expect("Should not fail to get member");

                match (&new_state, m) {
                    (MembershipState::Leave, None) => {}
                    (_, None) => {
                        panic!("Member should exist")
                    }
                    (_, Some(m)) => {
                        assert_eq!(*m.membership(), new_state);
                    }
                }
            }

            async fn bob_is_pinned(&self) -> bool {
                !self.bob_crypto_other_identity().await.identity_needs_user_approval()
            }

            async fn bob_is_verified(&self) -> bool {
                self.bob_crypto_other_identity().await.is_verified()
            }

            async fn bob_crypto_other_identity(&self) -> OtherUserIdentity {
                self.bob_user_identity()
                    .await
                    .expect("User identity should exist")
                    .underlying_identity()
                    .other()
                    .expect("Identity should be Other, not Own")
            }

            async fn bob_user_identity(&self) -> Option<UserIdentity> {
                self.client
                    .encryption()
                    .get_user_identity(&self.bob_user_id)
                    .await
                    .expect("Should not fail to get user identity")
            }
        }

        async fn create_just_me_room(
            client: &Client,
            sync_response_builder: &mut SyncResponseBuilder,
        ) -> Room {
            let create_room_sync_response = sync_response_builder
                .add_joined_room(
                    JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID)
                        .add_state_event(StateTestEvent::Member),
                )
                .build_sync_response();
            client.process_sync(create_room_sync_response).await.unwrap();
            let room = client.get_room(&DEFAULT_TEST_ROOM_ID).expect("Room should exist");
            assert_eq!(room.state(), RoomState::Joined);
            room
        }

        async fn create_room_with_other_member(
            builder: &mut SyncResponseBuilder,
            client: &Client,
            other_user_id: &UserId,
        ) -> Room {
            let create_room_sync_response = builder
                .add_joined_room(
                    JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID)
                        .add_state_event(StateTestEvent::Member)
                        .add_state_event(StateTestEvent::Custom(sync_response_member(
                            other_user_id,
                            MembershipState::Join,
                        ))),
                )
                .build_sync_response();
            client.process_sync(create_room_sync_response).await.unwrap();
            let room = client.get_room(&DEFAULT_TEST_ROOM_ID).expect("Room should exist");
            room.inner.mark_members_synced();

            assert_eq!(room.state(), RoomState::Joined);
            assert_eq!(
                *room
                    .get_member_no_sync(other_user_id)
                    .await
                    .expect("Should not fail to get member")
                    .expect("Member should exist")
                    .membership(),
                MembershipState::Join
            );
            room
        }

        async fn create_client_and_server() -> (Client, MockServer) {
            let server = MockServer::start().await;
            mock_members_request(&server).await;
            mock_secret_storage_default_key(&server).await;
            let client = logged_in_client(Some(server.uri())).await;
            (client, server)
        }

        async fn mock_members_request(server: &MockServer) {
            Mock::given(method("GET"))
                .and(path_regex(r"^/_matrix/client/r0/rooms/.*/members"))
                .and(header("authorization", "Bearer 1234"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(&*test_json::members::MEMBERS),
                )
                .mount(server)
                .await;
        }

        async fn mock_secret_storage_default_key(server: &MockServer) {
            Mock::given(method("GET"))
                .and(path_regex(
                    r"^/_matrix/client/r0/user/.*/account_data/m.secret_storage.default_key",
                ))
                .and(header("authorization", "Bearer 1234"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .mount(server)
                .await;
        }

        fn sync_response_member(
            user_id: &UserId,
            membership: MembershipState,
        ) -> serde_json::Value {
            json!({
                "content": {
                    "membership": membership.to_string(),
                },
                "event_id": format!(
                    "$aa{}bb:localhost",
                    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() % 100_000
                ),
                "origin_server_ts": 1472735824,
                "sender": "@example:localhost",
                "state_key": user_id,
                "type": "m.room.member",
                "unsigned": {
                    "age": 1234
                }
            })
        }
    }
}
