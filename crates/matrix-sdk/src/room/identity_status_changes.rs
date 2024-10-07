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
#![cfg(all(feature = "e2e-encryption", not(target_arch = "wasm32")))]

use std::collections::BTreeMap;

use async_stream::stream;
use futures_core::Stream;
use futures_util::{stream_select, StreamExt};
use matrix_sdk_base::crypto::{IdentityStatusChange, RoomIdentityChange, RoomIdentityState};
use ruma::{events::room::member::SyncRoomMemberEvent, OwnedUserId, UserId};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::Room;
use crate::{
    encryption::identities::{IdentityUpdates, UserIdentity},
    event_handler::EventHandlerDropGuard,
    Client, Error, Result,
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
    /// Create a new stream of changes to the identity status of members of a
    /// room.
    ///
    /// The "status" of an identity changes when our level of trust in it
    /// changes.
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

        let mut state = IdentityStatusChanges {
            room_identity_state: RoomIdentityState::new(room).await,
            _drop_guard: drop_guard,
        };

        Ok(stream!({
            let current_state =
                filter_non_self(state.room_identity_state.current_state(), &own_user_id);
            if !current_state.is_empty() {
                yield current_state;
            }
            while let Some(item) = unprocessed_stream.next().await {
                let update = filter_non_self(
                    state.room_identity_state.process_change(item).await,
                    &own_user_id,
                );
                if !update.is_empty() {
                    yield update;
                }
            }
        }))
    }
}

fn filter_non_self(
    mut input: Vec<IdentityStatusChange>,
    own_user_id: &UserId,
) -> Vec<IdentityStatusChange> {
    input.retain(|change| change.user_id != own_user_id);
    input
}

fn combine_streams(
    identity_updates: impl Stream<Item = RoomIdentityChange> + Unpin,
    room_member_events: impl Stream<Item = RoomIdentityChange> + Unpin,
) -> impl Stream<Item = RoomIdentityChange> {
    stream_select!(identity_updates, room_member_events)
}

async fn wrap_identity_updates(client: &Client) -> Result<impl Stream<Item = RoomIdentityChange>> {
    Ok(client
        .encryption()
        .user_identities_stream()
        .await?
        .map(|item| RoomIdentityChange::IdentityUpdates(to_base_updates(item))))
}

fn to_base_updates(input: IdentityUpdates) -> matrix_sdk_base::crypto::store::IdentityUpdates {
    matrix_sdk_base::crypto::store::IdentityUpdates {
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
) -> (EventHandlerDropGuard, impl Stream<Item = RoomIdentityChange>) {
    let own_user_id = room.own_user_id().to_owned();
    let room_id = room.room_id();
    let (sender, receiver) = mpsc::channel(16);
    let handle =
        room.client.add_room_event_handler(room_id, move |event: SyncRoomMemberEvent| async move {
            if *event.state_key() == own_user_id {
                return;
            }
            let _: Result<_, _> = sender.send(RoomIdentityChange::SyncRoomMemberEvent(event)).await;
        });
    let drop_guard = room.client.event_handler_drop_guard(handle);
    (drop_guard, ReceiverStream::new(receiver))
}

#[cfg(test)]
mod tests {
    use std::{
        pin::{pin, Pin},
        time::Duration,
    };

    use futures_core::Stream;
    use futures_util::FutureExt;
    use matrix_sdk_base::crypto::{IdentityState, IdentityStatusChange};
    use matrix_sdk_test::async_test;
    use test_setup::TestSetup;
    use tokio_stream::{StreamExt, Timeout};

    #[async_test]
    async fn test_when_user_becomes_unpinned_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_member().await;

        // And Bob's identity is pinned
        t.pin().await;

        // And we are listening for identity changes
        let changes = t.subscribe_to_identity_status_changes().await;

        // When Bob becomes unpinned
        t.unpin().await;

        // Then we were notified about it
        let change = next_change(&mut pin!(changes)).await;
        assert_eq!(change[0].user_id, t.user_id());
        assert_eq!(change[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_user_becomes_pinned_we_report_it() {
        // Given a room containing us and Bob
        let t = TestSetup::new_room_with_other_member().await;

        // And Bob's identity is unpinned
        t.unpin().await;

        // And we are listening for identity changes
        let changes = t.subscribe_to_identity_status_changes().await;
        let mut changes = pin!(changes);

        // When Bob becomes pinned
        t.pin().await;

        // Then we were notified about the initial state of the room
        let change1 = next_change(&mut changes).await;
        assert_eq!(change1[0].user_id, t.user_id());
        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change1.len(), 1);

        // And the change when Bob became pinned
        let change2 = next_change(&mut changes).await;
        assert_eq!(change2[0].user_id, t.user_id());
        assert_eq!(change2[0].changed_to, IdentityState::Pinned);
        assert_eq!(change2.len(), 1);
    }

    #[async_test]
    async fn test_when_an_unpinned_user_joins_we_report_it() {
        // Given a room containing just us
        let mut t = TestSetup::new_just_me_room().await;

        // And Bob's identity is unpinned
        t.unpin().await;

        // And we are listening for identity changes
        let changes = t.subscribe_to_identity_status_changes().await;

        // When Bob joins the room
        t.join().await;

        // Then we were notified about it
        let change = next_change(&mut pin!(changes)).await;
        assert_eq!(change[0].user_id, t.user_id());
        assert_eq!(change[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change.len(), 1);
    }

    #[async_test]
    async fn test_when_a_pinned_user_joins_we_do_not_report() {
        // Given a room containing just us
        let mut t = TestSetup::new_just_me_room().await;

        // And Bob's identity is unpinned
        t.pin().await;

        // And we are listening for identity changes
        let changes = t.subscribe_to_identity_status_changes().await;
        let mut changes = pin!(changes);

        // When Bob joins the room
        t.join().await;

        // Then there is no notification
        tokio::time::sleep(Duration::from_millis(200)).await;
        let change = changes.next().now_or_never();
        assert!(change.is_none());
    }

    #[async_test]
    async fn test_when_an_unpinned_user_leaves_we_report_it() {
        // Given a room containing us and Bob
        let mut t = TestSetup::new_room_with_other_member().await;

        // And Bob's identity is unpinned
        t.unpin().await;

        // And we are listening for identity changes
        let changes = t.subscribe_to_identity_status_changes().await;
        let mut changes = pin!(changes);

        // When Bob leaves the room
        t.leave().await;

        // Then we were notified about the initial state of the room
        let change1 = next_change(&mut changes).await;
        assert_eq!(change1[0].user_id, t.user_id());
        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change1.len(), 1);

        // And we were notified about the change when the user left
        let change2 = next_change(&mut changes).await;
        // Note: the user left the room, but we see that as them "becoming pinned" i.e.
        // "you no longer need to notify about this user".
        assert_eq!(change2[0].user_id, t.user_id());
        assert_eq!(change2[0].changed_to, IdentityState::Pinned);
        assert_eq!(change2.len(), 1);
    }

    #[async_test]
    async fn test_multiple_identity_changes_are_reported() {
        // Given a room containing just us
        let mut t = TestSetup::new_just_me_room().await;

        // And Bob's identity is unpinned
        t.unpin().await;

        // And we are listening for identity changes
        let changes = t.subscribe_to_identity_status_changes().await;
        let mut changes = pin!(changes);

        // NOTE: below we pull the changes out of the subscription after each action.
        // This makes sure that the identity changes and membership changes are
        // properly ordered. If we pull them out later, the identity changes get
        // shifted forward because they rely on less-complex async stuff under
        // the hood. Calling next_change ends up winding the async
        // machinery sufficiently that the membership change and any subsequent events
        // have fully completed.

        // When Bob joins the room ...
        t.join().await;
        let change1 = next_change(&mut changes).await;

        // ... becomes pinned ...
        t.pin().await;
        let change2 = next_change(&mut changes).await;

        // ... leaves and joins again (ignored since they stay pinned) ...
        t.leave().await;
        t.join().await;

        // ... becomes unpinned ...
        t.unpin().await;
        let change3 = next_change(&mut changes).await;

        // ... and leaves.
        t.leave().await;
        let change4 = next_change(&mut changes).await;

        assert_eq!(change1[0].user_id, t.user_id());
        assert_eq!(change2[0].user_id, t.user_id());
        assert_eq!(change3[0].user_id, t.user_id());
        assert_eq!(change4[0].user_id, t.user_id());

        assert_eq!(change1[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change2[0].changed_to, IdentityState::Pinned);
        assert_eq!(change3[0].changed_to, IdentityState::PinViolation);
        assert_eq!(change4[0].changed_to, IdentityState::Pinned);

        assert_eq!(change1.len(), 1);
        assert_eq!(change2.len(), 1);
        assert_eq!(change3.len(), 1);
        assert_eq!(change4.len(), 1);
    }

    // TODO: I (andyb) haven't figured out how to test room membership changes that
    // affect our own user (they should not be shown). Specifically, I haven't
    // figure out how to get out own user into a non-pinned state.

    async fn next_change(
        changes: &mut Pin<&mut Timeout<impl Stream<Item = Vec<IdentityStatusChange>>>>,
    ) -> Vec<IdentityStatusChange> {
        changes
            .next()
            .await
            .expect("There should be an identity update")
            .expect("Should not time out")
    }

    mod test_setup {
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        use futures_core::Stream;
        use matrix_sdk_base::{
            crypto::{IdentityStatusChange, OtherUserIdentity},
            RoomState,
        };
        use matrix_sdk_test::{
            test_json::{self, keys_query_sets::IdentityChangeDataSet},
            JoinedRoomBuilder, StateTestEvent, SyncResponseBuilder, DEFAULT_TEST_ROOM_ID,
        };
        use ruma::{
            api::client::keys::get_keys, events::room::member::MembershipState, owned_user_id,
            OwnedUserId, TransactionId, UserId,
        };
        use serde_json::json;
        use tokio_stream::{StreamExt as _, Timeout};
        use wiremock::{
            matchers::{header, method, path_regex},
            Mock, MockServer, ResponseTemplate,
        };

        use crate::{
            encryption::identities::UserIdentity, test_utils::logged_in_client, Client, Room,
        };

        /// Sets up a client and a room and allows changing user identities and
        /// room memberships. Note: most methods e.g. [`TestSetup::user_id`] are
        /// talking about the OTHER user, not our own user. Only methods
        /// starting with `self_` are talking about this user.
        pub(super) struct TestSetup {
            client: Client,
            user_id: OwnedUserId,
            sync_response_builder: SyncResponseBuilder,
            room: Room,
        }

        impl TestSetup {
            pub(super) async fn new_just_me_room() -> Self {
                let (client, user_id, mut sync_response_builder) = Self::init().await;
                let room = create_just_me_room(&client, &mut sync_response_builder).await;
                Self { client, user_id, sync_response_builder, room }
            }

            pub(super) async fn new_room_with_other_member() -> Self {
                let (client, user_id, mut sync_response_builder) = Self::init().await;
                let room =
                    create_room_with_other_member(&mut sync_response_builder, &client, &user_id)
                        .await;
                Self { client, user_id, sync_response_builder, room }
            }

            pub(super) fn user_id(&self) -> &UserId {
                &self.user_id
            }

            pub(super) async fn pin(&self) {
                if self.user_identity().await.is_some() {
                    assert!(
                        !self.is_pinned().await,
                        "pin() called when the identity is already pinned!"
                    );

                    // Pin it
                    self.user_identity()
                        .await
                        .expect("User should exist")
                        .pin()
                        .await
                        .expect("Should not fail to pin");
                } else {
                    // There was no existing identity. Set one. It will be pinned by default.
                    self.change_identity(IdentityChangeDataSet::key_query_with_identity_a()).await;
                }

                // Sanity check: we are pinned
                assert!(self.is_pinned().await);
            }

            pub(super) async fn unpin(&self) {
                // Change/set their identity - this will unpin if they already had one.
                // If this was the first time we'd done this, they are now pinned.
                self.change_identity(IdentityChangeDataSet::key_query_with_identity_a()).await;

                if self.is_pinned().await {
                    // Change their identity. Now they are definitely unpinned
                    self.change_identity(IdentityChangeDataSet::key_query_with_identity_b()).await;
                }

                // Sanity: we are unpinned
                assert!(!self.is_pinned().await);
            }

            pub(super) async fn join(&mut self) {
                self.membership_change(MembershipState::Join).await;
            }

            pub(super) async fn leave(&mut self) {
                self.membership_change(MembershipState::Leave).await;
            }

            pub(super) async fn subscribe_to_identity_status_changes(
                &self,
            ) -> Timeout<impl Stream<Item = Vec<IdentityStatusChange>>> {
                self.room
                    .subscribe_to_identity_status_changes()
                    .await
                    .expect("Should be able to subscribe")
                    .timeout(Duration::from_secs(2))
            }

            async fn init() -> (Client, OwnedUserId, SyncResponseBuilder) {
                let (client, _server) = create_client_and_server().await;

                // Note: if you change the user_id, you will need to change lots of hard-coded
                // stuff inside IdentityChangeDataSet
                let user_id = owned_user_id!("@bob:localhost");

                let sync_response_builder = SyncResponseBuilder::default();

                (client, user_id, sync_response_builder)
            }

            async fn change_identity(
                &self,
                key_query_response: get_keys::v3::Response,
            ) -> OtherUserIdentity {
                self.client
                    .mark_request_as_sent(&TransactionId::new(), &key_query_response)
                    .await
                    .expect("Should not fail to send identity changes");

                self.crypto_other_identity().await
            }

            async fn membership_change(&mut self, new_state: MembershipState) {
                let sync_response = self
                    .sync_response_builder
                    .add_joined_room(JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID).add_state_event(
                        StateTestEvent::Custom(sync_response_member(
                            &self.user_id,
                            new_state.clone(),
                        )),
                    ))
                    .build_sync_response();
                self.room.client.process_sync(sync_response).await.unwrap();

                // Make sure the membership stuck as expected
                let m = self
                    .room
                    .get_member_no_sync(&self.user_id)
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
                };
            }

            async fn is_pinned(&self) -> bool {
                !self.crypto_other_identity().await.identity_needs_user_approval()
            }

            async fn crypto_other_identity(&self) -> OtherUserIdentity {
                self.user_identity()
                    .await
                    .expect("User identity should exist")
                    .underlying_identity()
                    .other()
                    .expect("Identity should be Other, not Own")
            }

            async fn user_identity(&self) -> Option<UserIdentity> {
                self.client
                    .encryption()
                    .get_user_identity(&self.user_id)
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
