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
use matrix_sdk_common::AsyncTraitDeps;
use ruma::MxcUri;

use super::EventCacheStoreError;
use crate::media::MediaRequest;

/// An abstract trait that can be used to implement different store backends
/// for the event cache of the SDK.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait EventCacheStore: AsyncTraitDeps {
    /// The error type used by this event cache store.
    type Error: fmt::Debug + Into<EventCacheStoreError>;

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

    /// Remove a media file's content from the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn remove_media_content(&self, request: &MediaRequest) -> Result<(), Self::Error>;

    /// Remove all the media files' content associated to an `MxcUri` from the
    /// media store.
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
