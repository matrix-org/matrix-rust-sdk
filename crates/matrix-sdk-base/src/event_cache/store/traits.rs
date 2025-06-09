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

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use matrix_sdk_common::{
    linked_chunk::{
        ChunkIdentifier, ChunkIdentifierGenerator, LinkedChunkId, Position, RawChunk, Update,
    },
    AsyncTraitDeps,
};
use ruma::{events::relation::RelationType, EventId, MxcUri, OwnedEventId, RoomId};

use super::{
    media::{IgnoreMediaRetentionPolicy, MediaRetentionPolicy},
    EventCacheStoreError,
};
use crate::{
    event_cache::{Event, Gap},
    media::MediaRequestParameters,
};

/// A default capacity for linked chunks, when manipulating in conjunction with
/// an `EventCacheStore` implementation.
// TODO: move back?
pub const DEFAULT_CHUNK_CAPACITY: usize = 128;

/// An abstract trait that can be used to implement different store backends
/// for the event cache of the SDK.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait EventCacheStore: AsyncTraitDeps {
    /// The error type used by this event cache store.
    type Error: fmt::Debug + Into<EventCacheStoreError>;

    /// Try to take a lock using the given store.
    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool, Self::Error>;

    /// An [`Update`] reflects an operation that has happened inside a linked
    /// chunk. The linked chunk is used by the event cache to store the events
    /// in-memory. This method aims at forwarding this update inside this store.
    async fn handle_linked_chunk_updates(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), Self::Error>;

    /// Remove all data tied to a given room from the cache.
    async fn remove_room(&self, room_id: &RoomId) -> Result<(), Self::Error> {
        // Right now, this means removing all the linked chunk. If implementations
        // override this behavior, they should *also* include this code.
        self.handle_linked_chunk_updates(LinkedChunkId::Room(room_id), vec![Update::Clear]).await
    }

    /// Return all the raw components of a linked chunk, so the caller may
    /// reconstruct the linked chunk later.
    #[doc(hidden)]
    async fn load_all_chunks(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<RawChunk<Event, Gap>>, Self::Error>;

    /// Load the last chunk of the `LinkedChunk` holding all events of the room
    /// identified by `room_id`.
    ///
    /// This is used to iteratively load events for the `EventCache`.
    async fn load_last_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<(Option<RawChunk<Event, Gap>>, ChunkIdentifierGenerator), Self::Error>;

    /// Load the chunk before the chunk identified by `before_chunk_identifier`
    /// of the `LinkedChunk` holding all events of the room identified by
    /// `room_id`
    ///
    /// This is used to iteratively load events for the `EventCache`.
    async fn load_previous_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        before_chunk_identifier: ChunkIdentifier,
    ) -> Result<Option<RawChunk<Event, Gap>>, Self::Error>;

    /// Clear persisted events for all the rooms.
    ///
    /// This will empty and remove all the linked chunks stored previously,
    /// using the above [`Self::handle_linked_chunk_updates`] methods. It
    /// must *also* delete all the events' content, if they were stored in a
    /// separate table.
    ///
    /// ⚠ This is meant only for super specific use cases, where there shouldn't
    /// be any live in-memory linked chunks. In general, prefer using
    /// `EventCache::clear_all_rooms()` from the common SDK crate.
    async fn clear_all_linked_chunks(&self) -> Result<(), Self::Error>;

    /// Given a set of event IDs, return the duplicated events along with their
    /// position if there are any.
    async fn filter_duplicated_events(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        events: Vec<OwnedEventId>,
    ) -> Result<Vec<(OwnedEventId, Position)>, Self::Error>;

    /// Find an event by its ID in a room.
    async fn find_event(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<Event>, Self::Error>;

    /// Find all the events that relate to a given event.
    ///
    /// Note: it doesn't process relations recursively: for instance, if
    /// requesting only thread events, it will NOT return the aggregated
    /// events affecting the returned events. It is the responsibility of
    /// the caller to do so, if needed.
    ///
    /// An additional filter can be provided to only retrieve related events for
    /// a certain relationship.
    async fn find_event_relations(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
        filter: Option<&[RelationType]>,
    ) -> Result<Vec<Event>, Self::Error>;

    /// Save an event, that might or might not be part of an existing linked
    /// chunk.
    ///
    /// If the event has no event id, it will not be saved, and the function
    /// must return an Ok result early.
    ///
    /// If the event was already stored with the same id, it must be replaced,
    /// without causing an error.
    async fn save_event(&self, room_id: &RoomId, event: Event) -> Result<(), Self::Error>;

    /// Add a media file's content in the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    ///
    /// * `content` - The content of the file.
    async fn add_media_content(
        &self,
        request: &MediaRequestParameters,
        content: Vec<u8>,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Self::Error>;

    /// Replaces the given media's content key with another one.
    ///
    /// This should be used whenever a temporary (local) MXID has been used, and
    /// it must now be replaced with its actual remote counterpart (after
    /// uploading some content, or creating an empty MXC URI).
    ///
    /// ⚠ No check is performed to ensure that the media formats are consistent,
    /// i.e. it's possible to update with a thumbnail key a media that was
    /// keyed as a file before. The caller is responsible of ensuring that
    /// the replacement makes sense, according to their use case.
    ///
    /// This should not raise an error when the `from` parameter points to an
    /// unknown media, and it should silently continue in this case.
    ///
    /// # Arguments
    ///
    /// * `from` - The previous `MediaRequest` of the file.
    ///
    /// * `to` - The new `MediaRequest` of the file.
    async fn replace_media_key(
        &self,
        from: &MediaRequestParameters,
        to: &MediaRequestParameters,
    ) -> Result<(), Self::Error>;

    /// Get a media file's content out of the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn get_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Remove a media file's content from the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn remove_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<(), Self::Error>;

    /// Get a media file's content associated to an `MxcUri` from the
    /// media store.
    ///
    /// In theory, there could be several files stored using the same URI and a
    /// different `MediaFormat`. This API is meant to be used with a media file
    /// that has only been stored with a single format.
    ///
    /// If there are several media files for a given URI in different formats,
    /// this API will only return one of them. Which one is left as an
    /// implementation detail.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the media file.
    async fn get_media_content_for_uri(&self, uri: &MxcUri)
        -> Result<Option<Vec<u8>>, Self::Error>;

    /// Remove all the media files' content associated to an `MxcUri` from the
    /// media store.
    ///
    /// This should not raise an error when the `uri` parameter points to an
    /// unknown media, and it should return an Ok result in this case.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the media files.
    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<(), Self::Error>;

    /// Set the `MediaRetentionPolicy` to use for deciding whether to store or
    /// keep media content.
    ///
    /// # Arguments
    ///
    /// * `policy` - The `MediaRetentionPolicy` to use.
    async fn set_media_retention_policy(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), Self::Error>;

    /// Get the current `MediaRetentionPolicy`.
    fn media_retention_policy(&self) -> MediaRetentionPolicy;

    /// Set whether the current [`MediaRetentionPolicy`] should be ignored for
    /// the media.
    ///
    /// The change will be taken into account in the next cleanup.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequestParameters` of the file.
    ///
    /// * `ignore_policy` - Whether the current `MediaRetentionPolicy` should be
    ///   ignored.
    async fn set_ignore_media_retention_policy(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Self::Error>;

    /// Clean up the media cache with the current `MediaRetentionPolicy`.
    ///
    /// If there is already an ongoing cleanup, this is a noop.
    async fn clean_up_media_cache(&self) -> Result<(), Self::Error>;
}

#[repr(transparent)]
struct EraseEventCacheStoreError<T>(T);

#[cfg(not(tarpaulin_include))]
impl<T: fmt::Debug> fmt::Debug for EraseEventCacheStoreError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl<T: EventCacheStore> EventCacheStore for EraseEventCacheStoreError<T> {
    type Error = EventCacheStoreError;

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool, Self::Error> {
        self.0.try_take_leased_lock(lease_duration_ms, key, holder).await.map_err(Into::into)
    }

    async fn handle_linked_chunk_updates(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), Self::Error> {
        self.0.handle_linked_chunk_updates(linked_chunk_id, updates).await.map_err(Into::into)
    }

    async fn load_all_chunks(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<RawChunk<Event, Gap>>, Self::Error> {
        self.0.load_all_chunks(linked_chunk_id).await.map_err(Into::into)
    }

    async fn load_last_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<(Option<RawChunk<Event, Gap>>, ChunkIdentifierGenerator), Self::Error> {
        self.0.load_last_chunk(linked_chunk_id).await.map_err(Into::into)
    }

    async fn load_previous_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        before_chunk_identifier: ChunkIdentifier,
    ) -> Result<Option<RawChunk<Event, Gap>>, Self::Error> {
        self.0
            .load_previous_chunk(linked_chunk_id, before_chunk_identifier)
            .await
            .map_err(Into::into)
    }

    async fn clear_all_linked_chunks(&self) -> Result<(), Self::Error> {
        self.0.clear_all_linked_chunks().await.map_err(Into::into)
    }

    async fn filter_duplicated_events(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        events: Vec<OwnedEventId>,
    ) -> Result<Vec<(OwnedEventId, Position)>, Self::Error> {
        self.0.filter_duplicated_events(linked_chunk_id, events).await.map_err(Into::into)
    }

    async fn find_event(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<Event>, Self::Error> {
        self.0.find_event(room_id, event_id).await.map_err(Into::into)
    }

    async fn find_event_relations(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
        filter: Option<&[RelationType]>,
    ) -> Result<Vec<Event>, Self::Error> {
        self.0.find_event_relations(room_id, event_id, filter).await.map_err(Into::into)
    }

    async fn save_event(&self, room_id: &RoomId, event: Event) -> Result<(), Self::Error> {
        self.0.save_event(room_id, event).await.map_err(Into::into)
    }

    async fn add_media_content(
        &self,
        request: &MediaRequestParameters,
        content: Vec<u8>,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        self.0.add_media_content(request, content, ignore_policy).await.map_err(Into::into)
    }

    async fn replace_media_key(
        &self,
        from: &MediaRequestParameters,
        to: &MediaRequestParameters,
    ) -> Result<(), Self::Error> {
        self.0.replace_media_key(from, to).await.map_err(Into::into)
    }

    async fn get_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        self.0.get_media_content(request).await.map_err(Into::into)
    }

    async fn remove_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<(), Self::Error> {
        self.0.remove_media_content(request).await.map_err(Into::into)
    }

    async fn get_media_content_for_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        self.0.get_media_content_for_uri(uri).await.map_err(Into::into)
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<(), Self::Error> {
        self.0.remove_media_content_for_uri(uri).await.map_err(Into::into)
    }

    async fn set_media_retention_policy(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        self.0.set_media_retention_policy(policy).await.map_err(Into::into)
    }

    fn media_retention_policy(&self) -> MediaRetentionPolicy {
        self.0.media_retention_policy()
    }

    async fn set_ignore_media_retention_policy(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        self.0.set_ignore_media_retention_policy(request, ignore_policy).await.map_err(Into::into)
    }

    async fn clean_up_media_cache(&self) -> Result<(), Self::Error> {
        self.0.clean_up_media_cache().await.map_err(Into::into)
    }
}

/// A type-erased [`EventCacheStore`].
pub type DynEventCacheStore = dyn EventCacheStore<Error = EventCacheStoreError>;

/// A type that can be type-erased into `Arc<dyn EventCacheStore>`.
///
/// This trait is not meant to be implemented directly outside
/// `matrix-sdk-base`, but it is automatically implemented for everything that
/// implements `EventCacheStore`.
pub trait IntoEventCacheStore {
    #[doc(hidden)]
    fn into_event_cache_store(self) -> Arc<DynEventCacheStore>;
}

impl<T> IntoEventCacheStore for T
where
    T: EventCacheStore + Sized + 'static,
{
    fn into_event_cache_store(self) -> Arc<DynEventCacheStore> {
        Arc::new(EraseEventCacheStoreError(self))
    }
}

// Turns a given `Arc<T>` into `Arc<DynEventCacheStore>` by attaching the
// `EventCacheStore` impl vtable of `EraseEventCacheStoreError<T>`.
impl<T> IntoEventCacheStore for Arc<T>
where
    T: EventCacheStore + 'static,
{
    fn into_event_cache_store(self) -> Arc<DynEventCacheStore> {
        let ptr: *const T = Arc::into_raw(self);
        let ptr_erased = ptr as *const EraseEventCacheStoreError<T>;
        // SAFETY: EraseEventCacheStoreError is repr(transparent) so T and
        //         EraseEventCacheStoreError<T> have the same layout and ABI
        unsafe { Arc::from_raw(ptr_erased) }
    }
}
