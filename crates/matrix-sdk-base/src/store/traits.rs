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

use std::{borrow::Borrow, collections::BTreeSet, fmt, sync::Arc};

use async_trait::async_trait;
use matrix_sdk_common::AsyncTraitDeps;
use ruma::{
    events::{
        presence::PresenceEvent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, EmptyStateKey, GlobalAccountDataEvent,
        GlobalAccountDataEventContent, GlobalAccountDataEventType, RedactContent,
        RedactedStateEventContent, RoomAccountDataEvent, RoomAccountDataEventContent,
        RoomAccountDataEventType, StateEventType, StaticEventContent, StaticStateEventContent,
    },
    serde::Raw,
    EventId, MxcUri, OwnedEventId, OwnedUserId, RoomId, UserId,
};

use super::{StateChanges, StoreError};
use crate::{
    deserialized_responses::{RawAnySyncOrStrippedState, RawMemberEvent, RawSyncOrStrippedState},
    media::MediaRequest,
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

    /// Get the user ids of members for a given room with the given memberships,
    /// for stripped and regular rooms alike.
    async fn get_user_ids(
        &self,
        room_id: &RoomId,
        memberships: RoomMemberships,
    ) -> Result<Vec<OwnedUserId>, Self::Error>;

    /// Get all the user ids of members that are in the invited state for a
    /// given room, for stripped and regular rooms alike.
    #[deprecated = "Use get_user_ids with RoomMemberships::INVITE instead."]
    async fn get_invited_user_ids(&self, room_id: &RoomId)
        -> Result<Vec<OwnedUserId>, Self::Error>;

    /// Get all the user ids of members that are in the joined state for a
    /// given room, for stripped and regular rooms alike.
    #[deprecated = "Use get_user_ids with RoomMemberships::JOIN instead."]
    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>, Self::Error>;

    /// Get all the pure `RoomInfo`s the store knows about.
    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>, Self::Error>;

    /// Get all the pure `RoomInfo`s the store knows about.
    #[deprecated = "Use get_room_infos instead and filter by RoomState"]
    async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>, Self::Error>;

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

    /// Put arbitrary data into the custom store
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

    /// Remove arbitrary data from the custom store and return it if existed
    ///
    /// # Arguments
    ///
    /// * `key` - The key to remove data from
    async fn remove_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Add a media file's content in the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    ///
    /// * `content` - The content of the file.
    async fn add_media_content(
        &self,
        request: &MediaRequest,
        content: Vec<u8>,
    ) -> Result<(), Self::Error>;

    /// Get a media file's content out of the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn get_media_content(
        &self,
        request: &MediaRequest,
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Removes a media file's content from the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn remove_media_content(&self, request: &MediaRequest) -> Result<(), Self::Error>;

    /// Removes all the media files' content associated to an `MxcUri` from the
    /// media store.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the media files.
    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<(), Self::Error>;

    /// Removes a room and all elements associated from the state store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The `RoomId` of the room to delete.
    async fn remove_room(&self, room_id: &RoomId) -> Result<(), Self::Error>;
}

#[repr(transparent)]
struct EraseStateStoreError<T>(T);

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

    async fn get_user_ids(
        &self,
        room_id: &RoomId,
        memberships: RoomMemberships,
    ) -> Result<Vec<OwnedUserId>, Self::Error> {
        self.0.get_user_ids(room_id, memberships).await.map_err(Into::into)
    }

    async fn get_invited_user_ids(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<OwnedUserId>, Self::Error> {
        self.0.get_user_ids(room_id, RoomMemberships::INVITE).await.map_err(Into::into)
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>, Self::Error> {
        self.0.get_user_ids(room_id, RoomMemberships::JOIN).await.map_err(Into::into)
    }

    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>, Self::Error> {
        self.0.get_room_infos().await.map_err(Into::into)
    }

    #[allow(deprecated)]
    async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>, Self::Error> {
        self.0.get_stripped_room_infos().await.map_err(Into::into)
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<OwnedUserId>, Self::Error> {
        self.0.get_users_with_display_name(room_id, display_name).await.map_err(Into::into)
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

    async fn add_media_content(
        &self,
        request: &MediaRequest,
        content: Vec<u8>,
    ) -> Result<(), Self::Error> {
        self.0.add_media_content(request, content).await.map_err(Into::into)
    }

    async fn get_media_content(
        &self,
        request: &MediaRequest,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        self.0.get_media_content(request).await.map_err(Into::into)
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<(), Self::Error> {
        self.0.remove_media_content(request).await.map_err(Into::into)
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<(), Self::Error> {
        self.0.remove_media_content_for_uri(uri).await.map_err(Into::into)
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<(), Self::Error> {
        self.0.remove_room(room_id).await.map_err(Into::into)
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

/// A value for key-value data that should be persisted into the store.
#[derive(Debug, Clone)]
pub enum StateStoreDataValue {
    /// The sync token.
    SyncToken(String),

    /// A filter with the given ID.
    Filter(String),

    /// The user avatar url
    UserAvatarUrl(String),
}

impl StateStoreDataValue {
    /// Get this value if it is a sync token.
    pub fn into_sync_token(self) -> Option<String> {
        match self {
            Self::SyncToken(token) => Some(token),
            _ => None,
        }
    }

    /// Get this value if it is a filter.
    pub fn into_filter(self) -> Option<String> {
        match self {
            Self::Filter(filter) => Some(filter),
            _ => None,
        }
    }

    /// Get this value if it is a user avatar url.
    pub fn into_user_avatar_url(self) -> Option<String> {
        match self {
            Self::UserAvatarUrl(user_avatar_url) => Some(user_avatar_url),
            _ => None,
        }
    }
}

/// A key for key-value data.
#[derive(Debug, Clone, Copy)]
pub enum StateStoreDataKey<'a> {
    /// The sync token.
    SyncToken,

    /// A filter with the given name.
    Filter(&'a str),

    /// Avatar URL
    UserAvatarUrl(&'a UserId),
}

impl StateStoreDataKey<'_> {
    /// Key to use for the [`SyncToken`][Self::SyncToken] variant.
    pub const SYNC_TOKEN: &str = "sync_token";
    /// Key prefix to use for the [`Filter`][Self::Filter] variant.
    pub const FILTER: &str = "filter";
    /// Key prefix to use for the [`UserAvatarUrl`][Self::UserAvatarUrl]
    /// variant.
    pub const USER_AVATAR_URL: &str = "user_avatar_url";
}
