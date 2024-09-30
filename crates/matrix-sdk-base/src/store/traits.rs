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
    collections::{BTreeMap, BTreeSet},
    fmt,
    ops::Deref,
    sync::Arc,
};

use as_variant::as_variant;
use async_trait::async_trait;
use growable_bloom_filter::GrowableBloom;
use matrix_sdk_common::AsyncTraitDeps;
use ruma::{
    api::MatrixVersion,
    events::{
        presence::PresenceEvent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        AnyGlobalAccountDataEvent, AnyMessageLikeEventContent, AnyRoomAccountDataEvent,
        EmptyStateKey, EventContent as _, GlobalAccountDataEvent, GlobalAccountDataEventContent,
        GlobalAccountDataEventType, RawExt as _, RedactContent, RedactedStateEventContent,
        RoomAccountDataEvent, RoomAccountDataEventContent, RoomAccountDataEventType,
        StateEventType, StaticEventContent, StaticStateEventContent,
    },
    serde::Raw,
    time::SystemTime,
    EventId, OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedTransactionId, OwnedUserId, RoomId,
    TransactionId, UserId,
};
use serde::{Deserialize, Serialize};

use super::{StateChanges, StoreError};
use crate::{
    deserialized_responses::{RawAnySyncOrStrippedState, RawMemberEvent, RawSyncOrStrippedState},
    MinimalRoomMemberEvent, RoomInfo, RoomMemberships,
};

/// An abstract state store trait that can be used to implement different stores
/// for the SDK.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
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

    /// Get all the pure `RoomInfo`s the store knows about.
    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>, Self::Error>;

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
        display_name: &str,
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
        display_names: &'a [String],
    ) -> Result<BTreeMap<&'a str, BTreeSet<OwnedUserId>>, Self::Error>;

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

    /// Save an event to be sent by a send queue later.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the send queue's room.
    /// * `transaction_id` - The unique key identifying the event to be sent
    ///   (and its transaction). Note: this is expected to be randomly generated
    ///   and thus unique.
    /// * `content` - Serializable event content to be sent.
    async fn save_send_queue_event(
        &self,
        room_id: &RoomId,
        transaction_id: OwnedTransactionId,
        content: SerializableEventContent,
    ) -> Result<(), Self::Error>;

    /// Updates a send queue event with the given content, and resets its wedged
    /// status to false.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the send queue's room.
    /// * `transaction_id` - The unique key identifying the event to be sent
    ///   (and its transaction).
    /// * `content` - Serializable event content to replace the original one.
    ///
    /// Returns true if an event has been updated, or false otherwise.
    async fn update_send_queue_event(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
        content: SerializableEventContent,
    ) -> Result<bool, Self::Error>;

    /// Remove an event previously inserted with [`Self::save_send_queue_event`]
    /// from the database, based on its transaction id.
    ///
    /// Returns true if an event has been removed, or false otherwise.
    async fn remove_send_queue_event(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
    ) -> Result<bool, Self::Error>;

    /// Loads all the send queue events for the given room.
    async fn load_send_queue_events(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<QueuedEvent>, Self::Error>;

    /// Updates the send queue wedged status for a given send queue event.
    async fn update_send_queue_event_status(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
        wedged: bool,
    ) -> Result<(), Self::Error>;

    /// Loads all the rooms which have any pending events in their send queue.
    async fn load_rooms_with_unsent_events(&self) -> Result<Vec<OwnedRoomId>, Self::Error>;

    /// Add a new entry to the list of dependent send queue event for an event.
    async fn save_dependent_send_queue_event(
        &self,
        room_id: &RoomId,
        parent_txn_id: &TransactionId,
        own_txn_id: ChildTransactionId,
        content: DependentQueuedEventKind,
    ) -> Result<(), Self::Error>;

    /// Update a set of dependent send queue events with an event id,
    /// effectively marking them as ready.
    ///
    /// Returns the number of updated events.
    async fn update_dependent_send_queue_event(
        &self,
        room_id: &RoomId,
        parent_txn_id: &TransactionId,
        event_id: OwnedEventId,
    ) -> Result<usize, Self::Error>;

    /// Remove a specific dependent send queue event by id.
    ///
    /// Returns true if the dependent send queue event has been indeed removed.
    async fn remove_dependent_send_queue_event(
        &self,
        room: &RoomId,
        own_txn_id: &ChildTransactionId,
    ) -> Result<bool, Self::Error>;

    /// List all the dependent send queue events.
    ///
    /// This returns absolutely all the dependent send queue events, whether
    /// they have an event id or not. They must be returned in insertion order.
    async fn list_dependent_send_queue_events(
        &self,
        room: &RoomId,
    ) -> Result<Vec<DependentQueuedEvent>, Self::Error>;
}

#[repr(transparent)]
struct EraseStateStoreError<T>(T);

#[cfg(not(tarpaulin_include))]
impl<T: fmt::Debug> fmt::Debug for EraseStateStoreError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
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

    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>, Self::Error> {
        self.0.get_room_infos().await.map_err(Into::into)
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<OwnedUserId>, Self::Error> {
        self.0.get_users_with_display_name(room_id, display_name).await.map_err(Into::into)
    }

    async fn get_users_with_display_names<'a>(
        &self,
        room_id: &RoomId,
        display_names: &'a [String],
    ) -> Result<BTreeMap<&'a str, BTreeSet<OwnedUserId>>, Self::Error> {
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

    async fn save_send_queue_event(
        &self,
        room_id: &RoomId,
        transaction_id: OwnedTransactionId,
        content: SerializableEventContent,
    ) -> Result<(), Self::Error> {
        self.0.save_send_queue_event(room_id, transaction_id, content).await.map_err(Into::into)
    }

    async fn update_send_queue_event(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
        content: SerializableEventContent,
    ) -> Result<bool, Self::Error> {
        self.0.update_send_queue_event(room_id, transaction_id, content).await.map_err(Into::into)
    }

    async fn remove_send_queue_event(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
    ) -> Result<bool, Self::Error> {
        self.0.remove_send_queue_event(room_id, transaction_id).await.map_err(Into::into)
    }

    async fn load_send_queue_events(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<QueuedEvent>, Self::Error> {
        self.0.load_send_queue_events(room_id).await.map_err(Into::into)
    }

    async fn update_send_queue_event_status(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
        wedged: bool,
    ) -> Result<(), Self::Error> {
        self.0
            .update_send_queue_event_status(room_id, transaction_id, wedged)
            .await
            .map_err(Into::into)
    }

    async fn load_rooms_with_unsent_events(&self) -> Result<Vec<OwnedRoomId>, Self::Error> {
        self.0.load_rooms_with_unsent_events().await.map_err(Into::into)
    }

    async fn save_dependent_send_queue_event(
        &self,
        room_id: &RoomId,
        parent_txn_id: &TransactionId,
        own_txn_id: ChildTransactionId,
        content: DependentQueuedEventKind,
    ) -> Result<(), Self::Error> {
        self.0
            .save_dependent_send_queue_event(room_id, parent_txn_id, own_txn_id, content)
            .await
            .map_err(Into::into)
    }

    async fn update_dependent_send_queue_event(
        &self,
        room_id: &RoomId,
        parent_txn_id: &TransactionId,
        event_id: OwnedEventId,
    ) -> Result<usize, Self::Error> {
        self.0
            .update_dependent_send_queue_event(room_id, parent_txn_id, event_id)
            .await
            .map_err(Into::into)
    }

    async fn remove_dependent_send_queue_event(
        &self,
        room_id: &RoomId,
        own_txn_id: &ChildTransactionId,
    ) -> Result<bool, Self::Error> {
        self.0.remove_dependent_send_queue_event(room_id, own_txn_id).await.map_err(Into::into)
    }

    async fn list_dependent_send_queue_events(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<DependentQueuedEvent>, Self::Error> {
        self.0.list_dependent_send_queue_events(room_id).await.map_err(Into::into)
    }
}

/// Convenience functionality for state stores.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
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
        C: StaticEventContent + StaticStateEventContent<StateKey = EmptyStateKey> + RedactContent,
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
        C: StaticEventContent + StaticStateEventContent + RedactContent,
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
        C: StaticEventContent + StaticStateEventContent + RedactContent,
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
        C: StaticEventContent + StaticStateEventContent + RedactContent,
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
        C: StaticEventContent + GlobalAccountDataEventContent,
    {
        Ok(self.get_account_data_event(C::TYPE.into()).await?.map(Raw::cast))
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
        C: StaticEventContent + RoomAccountDataEventContent,
    {
        Ok(self.get_room_account_data_event(room_id, C::TYPE.into()).await?.map(Raw::cast))
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

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
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

/// Server capabilities returned by the /client/versions endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerCapabilities {
    /// Versions supported by the remote server.
    ///
    /// This contains [`MatrixVersion`]s converted to strings.
    pub versions: Vec<String>,

    /// List of unstable features and their enablement status.
    pub unstable_features: BTreeMap<String, bool>,

    /// Last time we fetched this data from the server, in milliseconds since
    /// epoch.
    last_fetch_ts: f64,
}

impl ServerCapabilities {
    /// The number of milliseconds after which the data is considered stale.
    pub const STALE_THRESHOLD: f64 = (1000 * 60 * 60 * 24 * 7) as _; // seven days

    /// Encode server capabilities into this serializable struct.
    pub fn new(versions: &[MatrixVersion], unstable_features: BTreeMap<String, bool>) -> Self {
        Self {
            versions: versions.iter().map(|item| item.to_string()).collect(),
            unstable_features,
            last_fetch_ts: now_timestamp_ms(),
        }
    }

    /// Decode server capabilities from this serializable struct.
    ///
    /// May return `None` if the data is considered stale, after
    /// [`Self::STALE_THRESHOLD`] milliseconds since the last time we stored
    /// it.
    pub fn maybe_decode(&self) -> Option<(Vec<MatrixVersion>, BTreeMap<String, bool>)> {
        if now_timestamp_ms() - self.last_fetch_ts >= Self::STALE_THRESHOLD {
            None
        } else {
            Some((
                self.versions.iter().filter_map(|item| item.parse().ok()).collect(),
                self.unstable_features.clone(),
            ))
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

    /// The server capabilities.
    ServerCapabilities(ServerCapabilities),

    /// A filter with the given ID.
    Filter(String),

    /// The user avatar url
    UserAvatarUrl(OwnedMxcUri),

    /// A list of recently visited room identifiers for the current user
    RecentlyVisitedRooms(Vec<OwnedRoomId>),

    /// Persistent data for
    /// `matrix_sdk_ui::unable_to_decrypt_hook::UtdHookManager`.
    UtdHookManagerData(GrowableBloom),

    /// A composer draft for the room.
    /// To learn more, see [`ComposerDraft`].
    ///
    /// [`ComposerDraft`]: Self::ComposerDraft
    ComposerDraft(ComposerDraft),
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

    /// Get this value if it is the server capabilities metadata.
    pub fn into_server_capabilities(self) -> Option<ServerCapabilities> {
        as_variant!(self, Self::ServerCapabilities)
    }
}

/// A key for key-value data.
#[derive(Debug, Clone, Copy)]
pub enum StateStoreDataKey<'a> {
    /// The sync token.
    SyncToken,

    /// The server capabilities,
    ServerCapabilities,

    /// A filter with the given name.
    Filter(&'a str),

    /// Avatar URL
    UserAvatarUrl(&'a UserId),

    /// Recently visited room identifiers
    RecentlyVisitedRooms(&'a UserId),

    /// Persistent data for
    /// `matrix_sdk_ui::unable_to_decrypt_hook::UtdHookManager`.
    UtdHookManagerData,

    /// A composer draft for the room.
    /// To learn more, see [`ComposerDraft`].
    ///
    /// [`ComposerDraft`]: Self::ComposerDraft
    ComposerDraft(&'a RoomId),
}

impl StateStoreDataKey<'_> {
    /// Key to use for the [`SyncToken`][Self::SyncToken] variant.
    pub const SYNC_TOKEN: &'static str = "sync_token";
    /// Key to use for the [`ServerCapabilities`][Self::ServerCapabilities]
    /// variant.
    pub const SERVER_CAPABILITIES: &'static str = "server_capabilities";
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

    /// Key prefix to use for the [`ComposerDraft`][Self::ComposerDraft]
    /// variant.
    pub const COMPOSER_DRAFT: &'static str = "composer_draft";
}

/// A thin wrapper to serialize a `AnyMessageLikeEventContent`.
#[derive(Clone, Serialize, Deserialize)]
pub struct SerializableEventContent {
    event: Raw<AnyMessageLikeEventContent>,
    event_type: String,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SerializableEventContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Don't include the event in the debug display.
        f.debug_struct("SerializedEventContent")
            .field("event_type", &self.event_type)
            .finish_non_exhaustive()
    }
}

impl SerializableEventContent {
    /// Create a [`SerializableEventContent`] from a raw
    /// [`AnyMessageLikeEventContent`] along with its type.
    pub fn from_raw(event: Raw<AnyMessageLikeEventContent>, event_type: String) -> Self {
        Self { event_type, event }
    }

    /// Create a [`SerializableEventContent`] from an
    /// [`AnyMessageLikeEventContent`].
    pub fn new(event: &AnyMessageLikeEventContent) -> Result<Self, serde_json::Error> {
        Ok(Self::from_raw(Raw::new(event)?, event.event_type().to_string()))
    }

    /// Convert a [`SerializableEventContent`] back into a
    /// [`AnyMessageLikeEventContent`].
    pub fn deserialize(&self) -> Result<AnyMessageLikeEventContent, serde_json::Error> {
        self.event.deserialize_with_type(self.event_type.clone().into())
    }

    /// Returns the raw event content along with its type.
    ///
    /// Useful for callers manipulating custom events.
    pub fn raw(self) -> (Raw<AnyMessageLikeEventContent>, String) {
        (self.event, self.event_type)
    }
}

/// An event to be sent with a send queue.
#[derive(Clone)]
pub struct QueuedEvent {
    /// The content of the message-like event we'd like to send.
    pub event: SerializableEventContent,

    /// Unique transaction id for the queued event, acting as a key.
    pub transaction_id: OwnedTransactionId,

    /// If the event couldn't be sent because of an API error, it's marked as
    /// wedged, and won't ever be peeked for sending. The only option is to
    /// remove it.
    pub is_wedged: bool,
}

/// The specific user intent that characterizes a [`DependentQueuedEvent`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DependentQueuedEventKind {
    /// The event should be edited.
    Edit {
        /// The new event for the content.
        new_content: SerializableEventContent,
    },

    /// The event should be redacted/aborted/removed.
    Redact,

    /// The event should be reacted to, with the given key.
    React {
        /// Key used for the reaction.
        key: String,
    },
}

/// A transaction id identifying a [`DependentQueuedEvent`] rather than its
/// parent [`QueuedEvent`].
///
/// This thin wrapper adds some safety to some APIs, making it possible to
/// distinguish between the parent's `TransactionId` and the dependent event's
/// own `TransactionId`.
#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChildTransactionId(OwnedTransactionId);

impl ChildTransactionId {
    /// Returns a new [`ChildTransactionId`].
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(TransactionId::new())
    }
}

impl Deref for ChildTransactionId {
    type Target = TransactionId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<String> for ChildTransactionId {
    fn from(val: String) -> Self {
        Self(val.into())
    }
}

impl From<ChildTransactionId> for OwnedTransactionId {
    fn from(val: ChildTransactionId) -> Self {
        val.0
    }
}

/// An event to be sent, depending on a [`QueuedEvent`] to be sent first.
///
/// Depending on whether the event has been sent or not, this will either update
/// the local echo in the storage, or send an event equivalent to the user
/// intent to the homeserver.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DependentQueuedEvent {
    /// Unique identifier for this dependent queued event.
    ///
    /// Useful for deletion.
    pub own_transaction_id: ChildTransactionId,

    /// The kind of user intent.
    pub kind: DependentQueuedEventKind,

    /// Transaction id for the parent's local echo / used in the server request.
    ///
    /// Note: this is the transaction id used for the depended-on event, i.e.
    /// the one that was originally sent and that's being modified with this
    /// dependent event.
    pub parent_transaction_id: OwnedTransactionId,

    /// If the parent event has been sent, the parent's event identifier
    /// returned by the server once the local echo has been sent out.
    ///
    /// Note: this is the event id used for the depended-on event after it's
    /// been sent, not for a possible event that could have been sent
    /// because of this [`DependentQueuedEvent`].
    pub event_id: Option<OwnedEventId>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for QueuedEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Hide the content from the debug log.
        f.debug_struct("QueuedEvent")
            .field("transaction_id", &self.transaction_id)
            .field("is_wedged", &self.is_wedged)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{now_timestamp_ms, ServerCapabilities};

    #[test]
    fn test_stale_server_capabilities() {
        let mut caps = ServerCapabilities {
            versions: Default::default(),
            unstable_features: Default::default(),
            last_fetch_ts: now_timestamp_ms() - ServerCapabilities::STALE_THRESHOLD - 1.0,
        };

        // Definitely stale.
        assert!(caps.maybe_decode().is_none());

        // Definitely not stale.
        caps.last_fetch_ts = now_timestamp_ms() - 1.0;
        assert!(caps.maybe_decode().is_some());
    }
}
