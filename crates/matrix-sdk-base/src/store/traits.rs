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

use std::{
    borrow::Borrow,
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    sync::Arc,
};

use as_variant::as_variant;
use async_trait::async_trait;
use growable_bloom_filter::GrowableBloom;
use matrix_sdk_common::AsyncTraitDeps;
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedMxcUri, OwnedRoomId,
    OwnedTransactionId, OwnedUserId, RoomId, TransactionId, UserId,
    api::{
        SupportedVersions,
        client::discovery::discover_homeserver::{
            self, HomeserverInfo, IdentityServerInfo, RtcFocusInfo, TileServerInfo,
        },
    },
    events::{
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, EmptyStateKey, GlobalAccountDataEvent,
        GlobalAccountDataEventContent, GlobalAccountDataEventType, RedactContent,
        RedactedStateEventContent, RoomAccountDataEvent, RoomAccountDataEventContent,
        RoomAccountDataEventType, StateEventType, StaticEventContent, StaticStateEventContent,
        presence::PresenceEvent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
    },
    serde::Raw,
    time::SystemTime,
};
use serde::{Deserialize, Serialize};

use super::{
    ChildTransactionId, DependentQueuedRequest, DependentQueuedRequestKind, QueueWedgeError,
    QueuedRequest, QueuedRequestKind, RoomLoadSettings, StateChanges, StoreError,
    send_queue::SentRequestKey,
};
use crate::{
    MinimalRoomMemberEvent, RoomInfo, RoomMemberships,
    deserialized_responses::{
        DisplayName, RawAnySyncOrStrippedState, RawMemberEvent, RawSyncOrStrippedState,
    },
    store::StoredThreadSubscription,
};

/// An abstract state store trait that can be used to implement different stores
/// for the SDK.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait StateStore: AsyncTraitDeps {
    /// The error type used by this state store.
    type Error: fmt::Debug + Into<StoreError> + From<serde_json::Error>;

    /// Get key-value data from the store.
    ///
    /// # Arguments
    ///
    /// * `key` - The key to fetch data for.
    async fn get_kv_data(
        &self,
        key: StateStoreDataKey<'_>,
    ) -> Result<Option<StateStoreDataValue>, Self::Error>;

    /// Put key-value data into the store.
    ///
    /// # Arguments
    ///
    /// * `key` - The key to identify the data in the store.
    ///
    /// * `value` - The data to insert.
    ///
    /// Panics if the key and value variants do not match.
    async fn set_kv_data(
        &self,
        key: StateStoreDataKey<'_>,
        value: StateStoreDataValue,
    ) -> Result<(), Self::Error>;

    /// Remove key-value data from the store.
    ///
    /// # Arguments
    ///
    /// * `key` - The key to remove the data for.
    async fn remove_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<(), Self::Error>;

    /// Save the set of state changes in the store.
    async fn save_changes(&self, changes: &StateChanges) -> Result<(), Self::Error>;

    /// Get the stored presence event for the given user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The id of the user for which we wish to fetch the presence
    /// event for.
    async fn get_presence_event(
        &self,
        user_id: &UserId,
    ) -> Result<Option<Raw<PresenceEvent>>, Self::Error>;

    /// Get the stored presence events for the given users.
    ///
    /// # Arguments
    ///
    /// * `user_ids` - The IDs of the users to fetch the presence events for.
    async fn get_presence_events(
        &self,
        user_ids: &[OwnedUserId],
    ) -> Result<Vec<Raw<PresenceEvent>>, Self::Error>;

    /// Get a state event out of the state store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room the state event was received for.
    ///
    /// * `event_type` - The event type of the state event.
    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<RawAnySyncOrStrippedState>, Self::Error>;

    /// Get a list of state events for a given room and `StateEventType`.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room to find events for.
    ///
    /// * `event_type` - The event type.
    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<RawAnySyncOrStrippedState>, Self::Error>;

    /// Get a list of state events for a given room, `StateEventType`, and the
    /// given state keys.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room to find events for.
    ///
    /// * `event_type` - The event type.
    ///
    /// * `state_keys` - The list of state keys to find.
    async fn get_state_events_for_keys(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_keys: &[&str],
    ) -> Result<Vec<RawAnySyncOrStrippedState>, Self::Error>;

    /// Get the current profile for the given user in the given room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The room id the profile is used in.
    ///
    /// * `user_id` - The id of the user the profile belongs to.
    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalRoomMemberEvent>, Self::Error>;

    /// Get the current profiles for the given users in the given room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The ID of the room the profiles are used in.
    ///
    /// * `user_ids` - The IDs of the users the profiles belong to.
    async fn get_profiles<'a>(
        &self,
        room_id: &RoomId,
        user_ids: &'a [OwnedUserId],
    ) -> Result<BTreeMap<&'a UserId, MinimalRoomMemberEvent>, Self::Error>;

    /// Get the user ids of members for a given room with the given memberships,
    /// for stripped and regular rooms alike.
    async fn get_user_ids(
        &self,
        room_id: &RoomId,
        memberships: RoomMemberships,
    ) -> Result<Vec<OwnedUserId>, Self::Error>;

    /// Get a set of pure `RoomInfo`s the store knows about.
    async fn get_room_infos(
        &self,
        room_load_settings: &RoomLoadSettings,
    ) -> Result<Vec<RoomInfo>, Self::Error>;

    /// Get all the users that use the given display name in the given room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the display name users should
    /// be fetched for.
    ///
    /// * `display_name` - The display name that the users use.
    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &DisplayName,
    ) -> Result<BTreeSet<OwnedUserId>, Self::Error>;

    /// Get all the users that use the given display names in the given room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The ID of the room to fetch the display names for.
    ///
    /// * `display_names` - The display names that the users use.
    async fn get_users_with_display_names<'a>(
        &self,
        room_id: &RoomId,
        display_names: &'a [DisplayName],
    ) -> Result<HashMap<&'a DisplayName, BTreeSet<OwnedUserId>>, Self::Error>;

    /// Get an event out of the account data store.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The event type of the account data event.
    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>, Self::Error>;

    /// Get an event out of the room account data store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the room account data event
    ///   should
    /// be fetched.
    ///
    /// * `event_type` - The event type of the room account data event.
    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>, Self::Error>;

    /// Get an event out of the user room receipt store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the receipt should be
    ///   fetched.
    ///
    /// * `receipt_type` - The type of the receipt.
    ///
    /// * `thread` - The thread containing this receipt.
    ///
    /// * `user_id` - The id of the user for who the receipt should be fetched.
    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>, Self::Error>;

    /// Get events out of the event room receipt store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the receipts should be
    ///   fetched.
    ///
    /// * `receipt_type` - The type of the receipts.
    ///
    /// * `thread` - The thread containing this receipt.
    ///
    /// * `event_id` - The id of the event for which the receipts should be
    ///   fetched.
    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>, Self::Error>;

    /// Get arbitrary data from the custom store
    ///
    /// # Arguments
    ///
    /// * `key` - The key to fetch data for
    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Put arbitrary data into the custom store, return the data previously
    /// stored
    ///
    /// # Arguments
    ///
    /// * `key` - The key to insert data into
    ///
    /// * `value` - The value to insert
    async fn set_custom_value(
        &self,
        key: &[u8],
        value: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Put arbitrary data into the custom store, do not attempt to read any
    /// previous data
    ///
    /// Optimization option for set_custom_values for stores that would perform
    /// better withouts the extra read and the caller not needing that data
    /// returned. Otherwise this just wraps around `set_custom_data` and
    /// discards the result.
    ///
    /// # Arguments
    ///
    /// * `key` - The key to insert data into
    ///
    /// * `value` - The value to insert
    async fn set_custom_value_no_read(
        &self,
        key: &[u8],
        value: Vec<u8>,
    ) -> Result<(), Self::Error> {
        self.set_custom_value(key, value).await.map(|_| ())
    }

    /// Remove arbitrary data from the custom store and return it if existed
    ///
    /// # Arguments
    ///
    /// * `key` - The key to remove data from
    async fn remove_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Remove a room and all elements associated from the state store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to delete.
    async fn remove_room(&self, room_id: &RoomId) -> Result<(), Self::Error>;

    /// Save a request to be sent by a send queue later (e.g. sending an event).
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the send queue's room.
    /// * `transaction_id` - The unique key identifying the event to be sent
    ///   (and its transaction). Note: this is expected to be randomly generated
    ///   and thus unique.
    /// * `content` - Serializable event content to be sent.
    async fn save_send_queue_request(
        &self,
        room_id: &RoomId,
        transaction_id: OwnedTransactionId,
        created_at: MilliSecondsSinceUnixEpoch,
        request: QueuedRequestKind,
        priority: usize,
    ) -> Result<(), Self::Error>;

    /// Updates a send queue request with the given content, and resets its
    /// error status.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the send queue's room.
    /// * `transaction_id` - The unique key identifying the request to be sent
    ///   (and its transaction).
    /// * `content` - Serializable event content to replace the original one.
    ///
    /// Returns true if a request has been updated, or false otherwise.
    async fn update_send_queue_request(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
        content: QueuedRequestKind,
    ) -> Result<bool, Self::Error>;

    /// Remove a request previously inserted with
    /// [`Self::save_send_queue_request`] from the database, based on its
    /// transaction id.
    ///
    /// Returns true if something has been removed, or false otherwise.
    async fn remove_send_queue_request(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
    ) -> Result<bool, Self::Error>;

    /// Loads all the send queue requests for the given room.
    ///
    /// The resulting vector of queued requests should be ordered from higher
    /// priority to lower priority, and respect the insertion order when
    /// priorities are equal.
    async fn load_send_queue_requests(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<QueuedRequest>, Self::Error>;

    /// Updates the send queue error status (wedge) for a given send queue
    /// request.
    async fn update_send_queue_request_status(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
        error: Option<QueueWedgeError>,
    ) -> Result<(), Self::Error>;

    /// Loads all the rooms which have any pending requests in their send queue.
    async fn load_rooms_with_unsent_requests(&self) -> Result<Vec<OwnedRoomId>, Self::Error>;

    /// Add a new entry to the list of dependent send queue requests for a
    /// parent request.
    async fn save_dependent_queued_request(
        &self,
        room_id: &RoomId,
        parent_txn_id: &TransactionId,
        own_txn_id: ChildTransactionId,
        created_at: MilliSecondsSinceUnixEpoch,
        content: DependentQueuedRequestKind,
    ) -> Result<(), Self::Error>;

    /// Mark a set of dependent send queue requests as ready, using a key
    /// identifying the homeserver's response.
    ///
    /// ⚠ Beware! There's no verification applied that the parent key type is
    /// compatible with the dependent event type. The invalid state may be
    /// lazily filtered out in `load_dependent_queued_requests`.
    ///
    /// Returns the number of updated requests.
    async fn mark_dependent_queued_requests_as_ready(
        &self,
        room_id: &RoomId,
        parent_txn_id: &TransactionId,
        sent_parent_key: SentRequestKey,
    ) -> Result<usize, Self::Error>;

    /// Update a dependent send queue request with the new content.
    ///
    /// Returns true if the request was found and could be updated.
    async fn update_dependent_queued_request(
        &self,
        room_id: &RoomId,
        own_transaction_id: &ChildTransactionId,
        new_content: DependentQueuedRequestKind,
    ) -> Result<bool, Self::Error>;

    /// Remove a specific dependent send queue request by id.
    ///
    /// Returns true if the dependent send queue request has been indeed
    /// removed.
    async fn remove_dependent_queued_request(
        &self,
        room: &RoomId,
        own_txn_id: &ChildTransactionId,
    ) -> Result<bool, Self::Error>;

    /// List all the dependent send queue requests.
    ///
    /// This returns absolutely all the dependent send queue requests, whether
    /// they have a parent event id or not. As a contract for implementors, they
    /// must be returned in insertion order.
    async fn load_dependent_queued_requests(
        &self,
        room: &RoomId,
    ) -> Result<Vec<DependentQueuedRequest>, Self::Error>;

    /// Inserts or updates multiple thread subscriptions.
    ///
    /// If the new thread subscription hasn't set a bumpstamp, and there was a
    /// previous subscription in the database with a bumpstamp, the existing
    /// bumpstamp is kept.
    ///
    /// If the new thread subscription has a bumpstamp that's lower than or
    /// equal to a previous one, the existing subscription is kept, i.e.
    /// this method must have no effect.
    async fn upsert_thread_subscriptions(
        &self,
        updates: Vec<(&RoomId, &EventId, StoredThreadSubscription)>,
    ) -> Result<(), Self::Error>;

    /// Remove a previous thread subscription for a given room and thread.
    ///
    /// Note: removing an unknown thread subscription is a no-op.
    async fn remove_thread_subscription(
        &self,
        room: &RoomId,
        thread_id: &EventId,
    ) -> Result<(), Self::Error>;

    /// Loads the current thread subscription for a given room and thread.
    ///
    /// Returns `None` if there was no entry for the given room/thread pair.
    async fn load_thread_subscription(
        &self,
        room: &RoomId,
        thread_id: &EventId,
    ) -> Result<Option<StoredThreadSubscription>, Self::Error>;

    /// Perform database optimizations if any are available, i.e. vacuuming in
    /// SQLite.
    ///
    /// /// **Warning:** this was added to check if SQLite fragmentation was the
    /// source of performance issues, **DO NOT use in production**.
    #[doc(hidden)]
    async fn optimize(&self) -> Result<(), Self::Error>;

    /// Returns the size of the store in bytes, if known.
    async fn get_size(&self) -> Result<Option<usize>, Self::Error>;
}

#[repr(transparent)]
struct EraseStateStoreError<T>(T);

#[cfg(not(tarpaulin_include))]
impl<T: fmt::Debug> fmt::Debug for EraseStateStoreError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl<T: StateStore> StateStore for EraseStateStoreError<T> {
    type Error = StoreError;

    async fn get_kv_data(
        &self,
        key: StateStoreDataKey<'_>,
    ) -> Result<Option<StateStoreDataValue>, Self::Error> {
        self.0.get_kv_data(key).await.map_err(Into::into)
    }

    async fn set_kv_data(
        &self,
        key: StateStoreDataKey<'_>,
        value: StateStoreDataValue,
    ) -> Result<(), Self::Error> {
        self.0.set_kv_data(key, value).await.map_err(Into::into)
    }

    async fn remove_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<(), Self::Error> {
        self.0.remove_kv_data(key).await.map_err(Into::into)
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<(), Self::Error> {
        self.0.save_changes(changes).await.map_err(Into::into)
    }

    async fn get_presence_event(
        &self,
        user_id: &UserId,
    ) -> Result<Option<Raw<PresenceEvent>>, Self::Error> {
        self.0.get_presence_event(user_id).await.map_err(Into::into)
    }

    async fn get_presence_events(
        &self,
        user_ids: &[OwnedUserId],
    ) -> Result<Vec<Raw<PresenceEvent>>, Self::Error> {
        self.0.get_presence_events(user_ids).await.map_err(Into::into)
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<RawAnySyncOrStrippedState>, Self::Error> {
        self.0.get_state_event(room_id, event_type, state_key).await.map_err(Into::into)
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<RawAnySyncOrStrippedState>, Self::Error> {
        self.0.get_state_events(room_id, event_type).await.map_err(Into::into)
    }

    async fn get_state_events_for_keys(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_keys: &[&str],
    ) -> Result<Vec<RawAnySyncOrStrippedState>, Self::Error> {
        self.0.get_state_events_for_keys(room_id, event_type, state_keys).await.map_err(Into::into)
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalRoomMemberEvent>, Self::Error> {
        self.0.get_profile(room_id, user_id).await.map_err(Into::into)
    }

    async fn get_profiles<'a>(
        &self,
        room_id: &RoomId,
        user_ids: &'a [OwnedUserId],
    ) -> Result<BTreeMap<&'a UserId, MinimalRoomMemberEvent>, Self::Error> {
        self.0.get_profiles(room_id, user_ids).await.map_err(Into::into)
    }

    async fn get_user_ids(
        &self,
        room_id: &RoomId,
        memberships: RoomMemberships,
    ) -> Result<Vec<OwnedUserId>, Self::Error> {
        self.0.get_user_ids(room_id, memberships).await.map_err(Into::into)
    }

    async fn get_room_infos(
        &self,
        room_load_settings: &RoomLoadSettings,
    ) -> Result<Vec<RoomInfo>, Self::Error> {
        self.0.get_room_infos(room_load_settings).await.map_err(Into::into)
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &DisplayName,
    ) -> Result<BTreeSet<OwnedUserId>, Self::Error> {
        self.0.get_users_with_display_name(room_id, display_name).await.map_err(Into::into)
    }

    async fn get_users_with_display_names<'a>(
        &self,
        room_id: &RoomId,
        display_names: &'a [DisplayName],
    ) -> Result<HashMap<&'a DisplayName, BTreeSet<OwnedUserId>>, Self::Error> {
        self.0.get_users_with_display_names(room_id, display_names).await.map_err(Into::into)
    }

    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>, Self::Error> {
        self.0.get_account_data_event(event_type).await.map_err(Into::into)
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>, Self::Error> {
        self.0.get_room_account_data_event(room_id, event_type).await.map_err(Into::into)
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>, Self::Error> {
        self.0
            .get_user_room_receipt_event(room_id, receipt_type, thread, user_id)
            .await
            .map_err(Into::into)
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>, Self::Error> {
        self.0
            .get_event_room_receipt_events(room_id, receipt_type, thread, event_id)
            .await
            .map_err(Into::into)
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        self.0.get_custom_value(key).await.map_err(Into::into)
    }

    async fn set_custom_value(
        &self,
        key: &[u8],
        value: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        self.0.set_custom_value(key, value).await.map_err(Into::into)
    }

    async fn remove_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        self.0.remove_custom_value(key).await.map_err(Into::into)
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<(), Self::Error> {
        self.0.remove_room(room_id).await.map_err(Into::into)
    }

    async fn save_send_queue_request(
        &self,
        room_id: &RoomId,
        transaction_id: OwnedTransactionId,
        created_at: MilliSecondsSinceUnixEpoch,
        content: QueuedRequestKind,
        priority: usize,
    ) -> Result<(), Self::Error> {
        self.0
            .save_send_queue_request(room_id, transaction_id, created_at, content, priority)
            .await
            .map_err(Into::into)
    }

    async fn update_send_queue_request(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
        content: QueuedRequestKind,
    ) -> Result<bool, Self::Error> {
        self.0.update_send_queue_request(room_id, transaction_id, content).await.map_err(Into::into)
    }

    async fn remove_send_queue_request(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
    ) -> Result<bool, Self::Error> {
        self.0.remove_send_queue_request(room_id, transaction_id).await.map_err(Into::into)
    }

    async fn load_send_queue_requests(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<QueuedRequest>, Self::Error> {
        self.0.load_send_queue_requests(room_id).await.map_err(Into::into)
    }

    async fn update_send_queue_request_status(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
        error: Option<QueueWedgeError>,
    ) -> Result<(), Self::Error> {
        self.0
            .update_send_queue_request_status(room_id, transaction_id, error)
            .await
            .map_err(Into::into)
    }

    async fn load_rooms_with_unsent_requests(&self) -> Result<Vec<OwnedRoomId>, Self::Error> {
        self.0.load_rooms_with_unsent_requests().await.map_err(Into::into)
    }

    async fn save_dependent_queued_request(
        &self,
        room_id: &RoomId,
        parent_txn_id: &TransactionId,
        own_txn_id: ChildTransactionId,
        created_at: MilliSecondsSinceUnixEpoch,
        content: DependentQueuedRequestKind,
    ) -> Result<(), Self::Error> {
        self.0
            .save_dependent_queued_request(room_id, parent_txn_id, own_txn_id, created_at, content)
            .await
            .map_err(Into::into)
    }

    async fn mark_dependent_queued_requests_as_ready(
        &self,
        room_id: &RoomId,
        parent_txn_id: &TransactionId,
        sent_parent_key: SentRequestKey,
    ) -> Result<usize, Self::Error> {
        self.0
            .mark_dependent_queued_requests_as_ready(room_id, parent_txn_id, sent_parent_key)
            .await
            .map_err(Into::into)
    }

    async fn remove_dependent_queued_request(
        &self,
        room_id: &RoomId,
        own_txn_id: &ChildTransactionId,
    ) -> Result<bool, Self::Error> {
        self.0.remove_dependent_queued_request(room_id, own_txn_id).await.map_err(Into::into)
    }

    async fn load_dependent_queued_requests(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<DependentQueuedRequest>, Self::Error> {
        self.0.load_dependent_queued_requests(room_id).await.map_err(Into::into)
    }

    async fn update_dependent_queued_request(
        &self,
        room_id: &RoomId,
        own_transaction_id: &ChildTransactionId,
        new_content: DependentQueuedRequestKind,
    ) -> Result<bool, Self::Error> {
        self.0
            .update_dependent_queued_request(room_id, own_transaction_id, new_content)
            .await
            .map_err(Into::into)
    }

    async fn upsert_thread_subscriptions(
        &self,
        updates: Vec<(&RoomId, &EventId, StoredThreadSubscription)>,
    ) -> Result<(), Self::Error> {
        self.0.upsert_thread_subscriptions(updates).await.map_err(Into::into)
    }

    async fn load_thread_subscription(
        &self,
        room: &RoomId,
        thread_id: &EventId,
    ) -> Result<Option<StoredThreadSubscription>, Self::Error> {
        self.0.load_thread_subscription(room, thread_id).await.map_err(Into::into)
    }

    async fn remove_thread_subscription(
        &self,
        room: &RoomId,
        thread_id: &EventId,
    ) -> Result<(), Self::Error> {
        self.0.remove_thread_subscription(room, thread_id).await.map_err(Into::into)
    }

    async fn optimize(&self) -> Result<(), Self::Error> {
        self.0.optimize().await.map_err(Into::into)
    }

    async fn get_size(&self) -> Result<Option<usize>, Self::Error> {
        self.0.get_size().await.map_err(Into::into)
    }
}

/// Convenience functionality for state stores.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait StateStoreExt: StateStore {
    /// Get a specific state event of statically-known type.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room the state event was received for.
    async fn get_state_event_static<C>(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<RawSyncOrStrippedState<C>>, Self::Error>
    where
        C: StaticEventContent<IsPrefix = ruma::events::False>
            + StaticStateEventContent<StateKey = EmptyStateKey>
            + RedactContent,
        C::Redacted: RedactedStateEventContent,
    {
        Ok(self.get_state_event(room_id, C::TYPE.into(), "").await?.map(|raw| raw.cast()))
    }

    /// Get a specific state event of statically-known type.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room the state event was received for.
    async fn get_state_event_static_for_key<C, K>(
        &self,
        room_id: &RoomId,
        state_key: &K,
    ) -> Result<Option<RawSyncOrStrippedState<C>>, Self::Error>
    where
        C: StaticEventContent<IsPrefix = ruma::events::False>
            + StaticStateEventContent
            + RedactContent,
        C::StateKey: Borrow<K>,
        C::Redacted: RedactedStateEventContent,
        K: AsRef<str> + ?Sized + Sync,
    {
        Ok(self
            .get_state_event(room_id, C::TYPE.into(), state_key.as_ref())
            .await?
            .map(|raw| raw.cast()))
    }

    /// Get a list of state events of a statically-known type for a given room.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room to find events for.
    async fn get_state_events_static<C>(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<RawSyncOrStrippedState<C>>, Self::Error>
    where
        C: StaticEventContent<IsPrefix = ruma::events::False>
            + StaticStateEventContent
            + RedactContent,
        C::Redacted: RedactedStateEventContent,
    {
        // FIXME: Could be more efficient, if we had streaming store accessor functions
        Ok(self
            .get_state_events(room_id, C::TYPE.into())
            .await?
            .into_iter()
            .map(|raw| raw.cast())
            .collect())
    }

    /// Get a list of state events of a statically-known type for a given room
    /// and given state keys.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room to find events for.
    ///
    /// * `state_keys` - The list of state keys to find.
    async fn get_state_events_for_keys_static<'a, C, K, I>(
        &self,
        room_id: &RoomId,
        state_keys: I,
    ) -> Result<Vec<RawSyncOrStrippedState<C>>, Self::Error>
    where
        C: StaticEventContent<IsPrefix = ruma::events::False>
            + StaticStateEventContent
            + RedactContent,
        C::StateKey: Borrow<K>,
        C::Redacted: RedactedStateEventContent,
        K: AsRef<str> + Sized + Sync + 'a,
        I: IntoIterator<Item = &'a K> + Send,
        I::IntoIter: Send,
    {
        Ok(self
            .get_state_events_for_keys(
                room_id,
                C::TYPE.into(),
                &state_keys.into_iter().map(|k| k.as_ref()).collect::<Vec<_>>(),
            )
            .await?
            .into_iter()
            .map(|raw| raw.cast())
            .collect())
    }

    /// Get an event of a statically-known type from the account data store.
    async fn get_account_data_event_static<C>(
        &self,
    ) -> Result<Option<Raw<GlobalAccountDataEvent<C>>>, Self::Error>
    where
        C: StaticEventContent<IsPrefix = ruma::events::False> + GlobalAccountDataEventContent,
    {
        Ok(self.get_account_data_event(C::TYPE.into()).await?.map(Raw::cast_unchecked))
    }

    /// Get an event of a statically-known type from the room account data
    /// store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which the room account data event
    ///   should be fetched.
    async fn get_room_account_data_event_static<C>(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<Raw<RoomAccountDataEvent<C>>>, Self::Error>
    where
        C: StaticEventContent<IsPrefix = ruma::events::False> + RoomAccountDataEventContent,
    {
        Ok(self
            .get_room_account_data_event(room_id, C::TYPE.into())
            .await?
            .map(Raw::cast_unchecked))
    }

    /// Get the `MemberEvent` for the given state key in the given room id.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The room id the member event belongs to.
    ///
    /// * `state_key` - The user id that the member event defines the state for.
    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<RawMemberEvent>, Self::Error> {
        self.get_state_event_static_for_key(room_id, state_key).await
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl<T: StateStore + ?Sized> StateStoreExt for T {}

/// A type-erased [`StateStore`].
pub type DynStateStore = dyn StateStore<Error = StoreError>;

/// A type that can be type-erased into `Arc<dyn StateStore>`.
///
/// This trait is not meant to be implemented directly outside
/// `matrix-sdk-crypto`, but it is automatically implemented for everything that
/// implements `StateStore`.
pub trait IntoStateStore {
    #[doc(hidden)]
    fn into_state_store(self) -> Arc<DynStateStore>;
}

impl<T> IntoStateStore for T
where
    T: StateStore + Sized + 'static,
{
    fn into_state_store(self) -> Arc<DynStateStore> {
        Arc::new(EraseStateStoreError(self))
    }
}

// Turns a given `Arc<T>` into `Arc<DynStateStore>` by attaching the
// StateStore impl vtable of `EraseStateStoreError<T>`.
impl<T> IntoStateStore for Arc<T>
where
    T: StateStore + 'static,
{
    fn into_state_store(self) -> Arc<DynStateStore> {
        let ptr: *const T = Arc::into_raw(self);
        let ptr_erased = ptr as *const EraseStateStoreError<T>;
        // SAFETY: EraseStateStoreError is repr(transparent) so T and
        //         EraseStateStoreError<T> have the same layout and ABI
        unsafe { Arc::from_raw(ptr_erased) }
    }
}

/// A TTL value in the store whose data can only be accessed before it expires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtlStoreValue<T> {
    /// The data of the item.
    #[serde(flatten)]
    data: T,

    /// Last time we fetched this data from the server, in milliseconds since
    /// epoch.
    last_fetch_ts: f64,
}

impl<T> TtlStoreValue<T> {
    /// The number of milliseconds after which the data is considered stale.
    pub const STALE_THRESHOLD: f64 = (1000 * 60 * 60 * 24 * 7) as _; // seven days

    /// Construct a new `TtlStoreValue` with the given data.
    pub fn new(data: T) -> Self {
        Self { data, last_fetch_ts: now_timestamp_ms() }
    }

    /// Get the data of this value, if it hasn't expired.
    pub fn into_data(self) -> Option<T> {
        if now_timestamp_ms() - self.last_fetch_ts >= Self::STALE_THRESHOLD {
            None
        } else {
            Some(self.data)
        }
    }
}

/// Serialisable representation of get_supported_versions::Response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SupportedVersionsResponse {
    /// Versions supported by the remote server.
    pub versions: Vec<String>,

    /// List of unstable features and their enablement status.
    pub unstable_features: BTreeMap<String, bool>,
}

impl SupportedVersionsResponse {
    /// Extracts known Matrix versions and features from the un-typed lists of
    /// strings.
    ///
    /// Note: Matrix versions and features that Ruma cannot parse, or does not
    /// know about, are discarded.
    pub fn supported_versions(&self) -> SupportedVersions {
        SupportedVersions::from_parts(&self.versions, &self.unstable_features)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
/// A serialisable representation of discover_homeserver::Response.
pub struct WellKnownResponse {
    /// Information about the homeserver to connect to.
    pub homeserver: HomeserverInfo,

    /// Information about the identity server to connect to.
    pub identity_server: Option<IdentityServerInfo>,

    /// Information about the tile server to use to display location data.
    pub tile_server: Option<TileServerInfo>,

    /// A list of the available MatrixRTC foci, ordered by priority.
    pub rtc_foci: Vec<RtcFocusInfo>,
}

impl From<discover_homeserver::Response> for WellKnownResponse {
    fn from(response: discover_homeserver::Response) -> Self {
        Self {
            homeserver: response.homeserver,
            identity_server: response.identity_server,
            tile_server: response.tile_server,
            rtc_foci: response.rtc_foci,
        }
    }
}

/// Get the current timestamp as the number of milliseconds since Unix Epoch.
fn now_timestamp_ms() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("System clock was before 1970.")
        .as_secs_f64()
        * 1000.0
}

/// A value for key-value data that should be persisted into the store.
#[derive(Debug, Clone)]
pub enum StateStoreDataValue {
    /// The sync token.
    SyncToken(String),

    /// The supported versions of the server.
    SupportedVersions(TtlStoreValue<SupportedVersionsResponse>),

    /// The well-known information of the server.
    WellKnown(TtlStoreValue<Option<WellKnownResponse>>),

    /// A filter with the given ID.
    Filter(String),

    /// The user avatar url
    UserAvatarUrl(OwnedMxcUri),

    /// A list of recently visited room identifiers for the current user
    RecentlyVisitedRooms(Vec<OwnedRoomId>),

    /// Persistent data for
    /// `matrix_sdk_ui::unable_to_decrypt_hook::UtdHookManager`.
    UtdHookManagerData(GrowableBloom),

    /// A unit value telling us that the client uploaded duplicate one-time
    /// keys.
    OneTimeKeyAlreadyUploaded,

    /// A composer draft for the room.
    /// To learn more, see [`ComposerDraft`].
    ///
    /// [`ComposerDraft`]: Self::ComposerDraft
    ComposerDraft(ComposerDraft),

    /// A list of knock request ids marked as seen in a room.
    SeenKnockRequests(BTreeMap<OwnedEventId, OwnedUserId>),

    /// A list of tokens to continue thread subscriptions catchup.
    ///
    /// See documentation of [`ThreadSubscriptionCatchupToken`] for more
    /// details.
    ThreadSubscriptionsCatchupTokens(Vec<ThreadSubscriptionCatchupToken>),
}

/// Tokens to use when catching up on thread subscriptions.
///
/// These tokens are created when the client receives some thread subscriptions
/// from sync, but the sync indicates that there are more thread subscriptions
/// available on the server. In this case, it's expected that the client will
/// call the [MSC4308] companion endpoint to catch up (back-paginate) on
/// previous thread subscriptions.
///
/// [MSC4308]: https://github.com/matrix-org/matrix-spec-proposals/pull/4308
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadSubscriptionCatchupToken {
    /// The token to use as the lower bound when fetching new threads
    /// subscriptions.
    ///
    /// In sliding sync, this is the `prev_batch` value of a sliding sync
    /// response.
    pub from: String,

    /// The token to use as the upper bound when fetching new threads
    /// subscriptions.
    ///
    /// In sliding sync, it must be set to the `pos` value of the sliding sync
    /// *request*, which response received a `prev_batch` token.
    pub to: Option<String>,
}

/// Current draft of the composer for the room.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComposerDraft {
    /// The draft content in plain text.
    pub plain_text: String,
    /// If the message is formatted in HTML, the HTML representation of the
    /// message.
    pub html_text: Option<String>,
    /// The type of draft.
    pub draft_type: ComposerDraftType,
    /// Attachments associated with this draft.
    #[serde(default)]
    pub attachments: Vec<DraftAttachment>,
}

/// An attachment stored with a composer draft.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DraftAttachment {
    /// The filename of the attachment.
    pub filename: String,
    /// The attachment content with type-specific data.
    pub content: DraftAttachmentContent,
}

/// The content of a draft attachment with type-specific data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum DraftAttachmentContent {
    /// Image attachment.
    Image {
        /// The image file data.
        data: Vec<u8>,
        /// MIME type.
        mimetype: Option<String>,
        /// File size in bytes.
        size: Option<u64>,
        /// Width in pixels.
        width: Option<u64>,
        /// Height in pixels.
        height: Option<u64>,
        /// BlurHash string.
        blurhash: Option<String>,
        /// Optional thumbnail.
        thumbnail: Option<DraftThumbnail>,
    },
    /// Video attachment.
    Video {
        /// The video file data.
        data: Vec<u8>,
        /// MIME type.
        mimetype: Option<String>,
        /// File size in bytes.
        size: Option<u64>,
        /// Width in pixels.
        width: Option<u64>,
        /// Height in pixels.
        height: Option<u64>,
        /// Duration.
        duration: Option<std::time::Duration>,
        /// BlurHash string.
        blurhash: Option<String>,
        /// Optional thumbnail.
        thumbnail: Option<DraftThumbnail>,
    },
    /// Audio attachment.
    Audio {
        /// The audio file data.
        data: Vec<u8>,
        /// MIME type.
        mimetype: Option<String>,
        /// File size in bytes.
        size: Option<u64>,
        /// Duration.
        duration: Option<std::time::Duration>,
    },
    /// Generic file attachment.
    File {
        /// The file data.
        data: Vec<u8>,
        /// MIME type.
        mimetype: Option<String>,
        /// File size in bytes.
        size: Option<u64>,
    },
}

/// Thumbnail data for a draft attachment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DraftThumbnail {
    /// The filename of the thumbnail.
    pub filename: String,
    /// The thumbnail image data.
    pub data: Vec<u8>,
    /// MIME type of the thumbnail.
    pub mimetype: Option<String>,
    /// Width in pixels.
    pub width: Option<u64>,
    /// Height in pixels.
    pub height: Option<u64>,
    /// File size in bytes.
    pub size: Option<u64>,
}

/// The type of draft of the composer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ComposerDraftType {
    /// The draft is a new message.
    NewMessage,
    /// The draft is a reply to an event.
    Reply {
        /// The ID of the event being replied to.
        event_id: OwnedEventId,
    },
    /// The draft is an edit of an event.
    Edit {
        /// The ID of the event being edited.
        event_id: OwnedEventId,
    },
}

impl StateStoreDataValue {
    /// Get this value if it is a sync token.
    pub fn into_sync_token(self) -> Option<String> {
        as_variant!(self, Self::SyncToken)
    }

    /// Get this value if it is a filter.
    pub fn into_filter(self) -> Option<String> {
        as_variant!(self, Self::Filter)
    }

    /// Get this value if it is a user avatar url.
    pub fn into_user_avatar_url(self) -> Option<OwnedMxcUri> {
        as_variant!(self, Self::UserAvatarUrl)
    }

    /// Get this value if it is a list of recently visited rooms.
    pub fn into_recently_visited_rooms(self) -> Option<Vec<OwnedRoomId>> {
        as_variant!(self, Self::RecentlyVisitedRooms)
    }

    /// Get this value if it is the data for the `UtdHookManager`.
    pub fn into_utd_hook_manager_data(self) -> Option<GrowableBloom> {
        as_variant!(self, Self::UtdHookManagerData)
    }

    /// Get this value if it is a composer draft.
    pub fn into_composer_draft(self) -> Option<ComposerDraft> {
        as_variant!(self, Self::ComposerDraft)
    }

    /// Get this value if it is the supported versions metadata.
    pub fn into_supported_versions(self) -> Option<TtlStoreValue<SupportedVersionsResponse>> {
        as_variant!(self, Self::SupportedVersions)
    }

    /// Get this value if it is the well-known metadata.
    pub fn into_well_known(self) -> Option<TtlStoreValue<Option<WellKnownResponse>>> {
        as_variant!(self, Self::WellKnown)
    }

    /// Get this value if it is the data for the ignored join requests.
    pub fn into_seen_knock_requests(self) -> Option<BTreeMap<OwnedEventId, OwnedUserId>> {
        as_variant!(self, Self::SeenKnockRequests)
    }

    /// Get this value if it is the data for the thread subscriptions catchup
    /// tokens.
    pub fn into_thread_subscriptions_catchup_tokens(
        self,
    ) -> Option<Vec<ThreadSubscriptionCatchupToken>> {
        as_variant!(self, Self::ThreadSubscriptionsCatchupTokens)
    }
}

/// A key for key-value data.
#[derive(Debug, Clone, Copy)]
pub enum StateStoreDataKey<'a> {
    /// The sync token.
    SyncToken,

    /// The supported versions of the server,
    SupportedVersions,

    /// The well-known information of the server,
    WellKnown,

    /// A filter with the given name.
    Filter(&'a str),

    /// Avatar URL
    UserAvatarUrl(&'a UserId),

    /// Recently visited room identifiers
    RecentlyVisitedRooms(&'a UserId),

    /// Persistent data for
    /// `matrix_sdk_ui::unable_to_decrypt_hook::UtdHookManager`.
    UtdHookManagerData,

    /// Data remembering if the client already reported that it has uploaded
    /// duplicate one-time keys.
    OneTimeKeyAlreadyUploaded,

    /// A composer draft for the room.
    /// To learn more, see [`ComposerDraft`].
    ///
    /// [`ComposerDraft`]: Self::ComposerDraft
    ComposerDraft(&'a RoomId, Option<&'a EventId>),

    /// A list of knock request ids marked as seen in a room.
    SeenKnockRequests(&'a RoomId),

    /// A list of thread subscriptions catchup tokens.
    ThreadSubscriptionsCatchupTokens,
}

impl StateStoreDataKey<'_> {
    /// Key to use for the [`SyncToken`][Self::SyncToken] variant.
    pub const SYNC_TOKEN: &'static str = "sync_token";

    /// Key to use for the [`SupportedVersions`][Self::SupportedVersions]
    /// variant.
    pub const SUPPORTED_VERSIONS: &'static str = "server_capabilities"; // Note: this is the old name, kept for backwards compatibility.

    /// Key to use for the [`WellKnown`][Self::WellKnown]
    /// variant.
    pub const WELL_KNOWN: &'static str = "well_known";

    /// Key prefix to use for the [`Filter`][Self::Filter] variant.
    pub const FILTER: &'static str = "filter";

    /// Key prefix to use for the [`UserAvatarUrl`][Self::UserAvatarUrl]
    /// variant.
    pub const USER_AVATAR_URL: &'static str = "user_avatar_url";

    /// Key prefix to use for the
    /// [`RecentlyVisitedRooms`][Self::RecentlyVisitedRooms] variant.
    pub const RECENTLY_VISITED_ROOMS: &'static str = "recently_visited_rooms";

    /// Key to use for the [`UtdHookManagerData`][Self::UtdHookManagerData]
    /// variant.
    pub const UTD_HOOK_MANAGER_DATA: &'static str = "utd_hook_manager_data";

    /// Key to use for the flag remembering that we already reported that we
    /// uploaded duplicate one-time keys.
    pub const ONE_TIME_KEY_ALREADY_UPLOADED: &'static str = "one_time_key_already_uploaded";

    /// Key prefix to use for the [`ComposerDraft`][Self::ComposerDraft]
    /// variant.
    pub const COMPOSER_DRAFT: &'static str = "composer_draft";

    /// Key prefix to use for the
    /// [`SeenKnockRequests`][Self::SeenKnockRequests] variant.
    pub const SEEN_KNOCK_REQUESTS: &'static str = "seen_knock_requests";

    /// Key prefix to use for the
    /// [`ThreadSubscriptionsCatchupTokens`][Self::ThreadSubscriptionsCatchupTokens] variant.
    pub const THREAD_SUBSCRIPTIONS_CATCHUP_TOKENS: &'static str =
        "thread_subscriptions_catchup_tokens";
}

/// Compare two thread subscription changes bump stamps, given a fixed room and
/// thread root event id pair.
///
/// May update the newer one to keep the previous one if needed, under some
/// conditions.
///
/// Returns true if the new subscription should be stored, or false if the new
/// subscription should be ignored.
pub fn compare_thread_subscription_bump_stamps(
    previous: Option<u64>,
    new: &mut Option<u64>,
) -> bool {
    match (previous, &new) {
        // If the previous subscription had a bump stamp, and the new one doesn't, keep the
        // previous one; it should be updated soon via sync anyways.
        (Some(prev_bump), None) => {
            *new = Some(prev_bump);
        }

        // If the previous bump stamp is newer than the new one, don't store the value at all.
        (Some(prev_bump), Some(new_bump)) if *new_bump <= prev_bump => {
            return false;
        }

        // In all other cases, keep the new bumpstamp.
        _ => {}
    }

    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{SupportedVersionsResponse, TtlStoreValue, now_timestamp_ms};

    #[test]
    fn test_stale_ttl_store_value() {
        // Definitely stale.
        let ttl_value = TtlStoreValue {
            data: (),
            last_fetch_ts: now_timestamp_ms() - TtlStoreValue::<()>::STALE_THRESHOLD - 1.0,
        };
        assert!(ttl_value.into_data().is_none());

        // Definitely not stale.
        let ttl_value = TtlStoreValue::new(());
        assert!(ttl_value.into_data().is_some());
    }

    #[test]
    fn test_stale_ttl_store_value_serialize_roundtrip() {
        let server_info = SupportedVersionsResponse {
            versions: vec!["1.2".to_owned(), "1.3".to_owned(), "1.4".to_owned()],
            unstable_features: [("org.matrix.msc3916.stable".to_owned(), true)].into(),
        };
        let ttl_value = TtlStoreValue { data: server_info.clone(), last_fetch_ts: 1000.0 };
        let json = json!({
            "versions": ["1.2", "1.3", "1.4"],
            "unstable_features": {
                "org.matrix.msc3916.stable": true,
            },
            "last_fetch_ts": 1000.0,
        });

        assert_eq!(serde_json::to_value(&ttl_value).unwrap(), json);

        let deserialized =
            serde_json::from_value::<TtlStoreValue<SupportedVersionsResponse>>(json).unwrap();
        assert_eq!(deserialized.data, server_info);
        assert!(deserialized.last_fetch_ts - ttl_value.last_fetch_ts < 0.0001);
    }
}
