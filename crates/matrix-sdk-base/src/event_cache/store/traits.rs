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
    linked_chunk::{LinkedChunk, Update},
    AsyncTraitDeps,
};
use ruma::{MxcUri, RoomId};

use super::EventCacheStoreError;
use crate::{
    event_cache::{Event, Gap},
    media::MediaRequestParameters,
};

/// A default capacity for linked chunks, when manipulating in conjunction with
/// an `EventCacheStore` implementation.
pub const DEFAULT_CHUNK_CAPACITY: usize = 128;

/// An abstract trait that can be used to implement different store backends
/// for the event cache of the SDK.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
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
        room_id: &RoomId,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), Self::Error>;

    /// Reconstruct a full linked chunk by reloading it from storage.
    async fn reload_linked_chunk(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<LinkedChunk<DEFAULT_CHUNK_CAPACITY, Event, Gap>>, Self::Error>;

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
}

#[repr(transparent)]
struct EraseEventCacheStoreError<T>(T);

#[cfg(not(tarpaulin_include))]
impl<T: fmt::Debug> fmt::Debug for EraseEventCacheStoreError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
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
        room_id: &RoomId,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), Self::Error> {
        self.0.handle_linked_chunk_updates(room_id, updates).await.map_err(Into::into)
    }

    async fn reload_linked_chunk(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<LinkedChunk<DEFAULT_CHUNK_CAPACITY, Event, Gap>>, Self::Error> {
        self.0.reload_linked_chunk(room_id).await.map_err(Into::into)
    }

    async fn add_media_content(
        &self,
        request: &MediaRequestParameters,
        content: Vec<u8>,
    ) -> Result<(), Self::Error> {
        self.0.add_media_content(request, content).await.map_err(Into::into)
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

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<(), Self::Error> {
        self.0.remove_media_content_for_uri(uri).await.map_err(Into::into)
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
