// Copyright 2025 Kévin Commaille
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

//! Types and traits regarding media caching of the media store.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use matrix_sdk_common::{AsyncTraitDeps, cross_process_lock::CrossProcessLockGeneration};
use ruma::{MxcUri, time::SystemTime};

#[cfg(doc)]
use crate::media::store::MediaService;
use crate::media::{
    MediaRequestParameters,
    store::{IgnoreMediaRetentionPolicy, MediaRetentionPolicy, MediaStoreError},
};

/// An abstract trait that can be used to implement different store backends
/// for the media of the SDK.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait MediaStore: AsyncTraitDeps {
    /// The error type used by this media store.
    type Error: fmt::Debug + Into<MediaStoreError>;

    /// Try to take a lock using the given store.
    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<CrossProcessLockGeneration>, Self::Error>;

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
    async fn clean(&self) -> Result<(), Self::Error>;

    /// Perform database optimizations if any are available, i.e. vacuuming in
    /// SQLite.
    ///
    /// **Warning:** this was added to check if SQLite fragmentation was the
    /// source of performance issues, **DO NOT use in production**.
    #[doc(hidden)]
    async fn optimize(&self) -> Result<(), Self::Error>;

    /// Returns the size of the store in bytes, if known.
    async fn get_size(&self) -> Result<Option<usize>, Self::Error>;
}

/// An abstract trait that can be used to implement different store backends
/// for the media cache of the SDK.
///
/// The main purposes of this trait are to be able to centralize where we handle
/// [`MediaRetentionPolicy`] by wrapping this in a [`MediaService`], and to
/// simplify the implementation of tests by being able to have complete control
/// over the `SystemTime`s provided to the store.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait MediaStoreInner: AsyncTraitDeps + Clone {
    /// The error type used by this media cache store.
    type Error: fmt::Debug + fmt::Display + Into<MediaStoreError>;

    /// The persisted media retention policy in the media cache.
    async fn media_retention_policy_inner(
        &self,
    ) -> Result<Option<MediaRetentionPolicy>, Self::Error>;

    /// Persist the media retention policy in the media cache.
    ///
    /// # Arguments
    ///
    /// * `policy` - The `MediaRetentionPolicy` to persist.
    async fn set_media_retention_policy_inner(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), Self::Error>;

    /// Add a media file's content in the media cache.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequestParameters` of the file.
    ///
    /// * `content` - The content of the file.
    ///
    /// * `current_time` - The current time, to set the last access time of the
    ///   media.
    ///
    /// * `policy` - The media retention policy, to check whether the media is
    ///   too big to be cached.
    ///
    /// * `ignore_policy` - Whether the `MediaRetentionPolicy` should be ignored
    ///   for this media. This setting should be persisted alongside the media
    ///   and taken into account whenever the policy is used.
    async fn add_media_content_inner(
        &self,
        request: &MediaRequestParameters,
        content: Vec<u8>,
        current_time: SystemTime,
        policy: MediaRetentionPolicy,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Self::Error>;

    /// Set whether the current [`MediaRetentionPolicy`] should be ignored for
    /// the media.
    ///
    /// If the media of the given request is not found, this should be a noop.
    ///
    /// The change will be taken into account in the next cleanup.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequestParameters` of the file.
    ///
    /// * `ignore_policy` - Whether the current `MediaRetentionPolicy` should be
    ///   ignored.
    async fn set_ignore_media_retention_policy_inner(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Self::Error>;

    /// Get a media file's content out of the media cache.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequestParameters` of the file.
    ///
    /// * `current_time` - The current time, to update the last access time of
    ///   the media.
    async fn get_media_content_inner(
        &self,
        request: &MediaRequestParameters,
        current_time: SystemTime,
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Get a media file's content associated to an `MxcUri` from the
    /// media store.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the media file.
    ///
    /// * `current_time` - The current time, to update the last access time of
    ///   the media.
    async fn get_media_content_for_uri_inner(
        &self,
        uri: &MxcUri,
        current_time: SystemTime,
    ) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Clean up the media cache with the given policy.
    ///
    /// For the integration tests, it is expected that content that does not
    /// pass the last access expiry and max file size criteria will be
    /// removed first. After that, the remaining cache size should be
    /// computed to compare against the max cache size criteria.
    ///
    /// # Arguments
    ///
    /// * `policy` - The media retention policy to use for the cleanup. The
    ///   `cleanup_frequency` will be ignored.
    ///
    /// * `current_time` - The current time, to be used to check for expired
    ///   content and to be stored as the time of the last media cache cleanup.
    async fn clean_inner(
        &self,
        policy: MediaRetentionPolicy,
        current_time: SystemTime,
    ) -> Result<(), Self::Error>;

    /// The time of the last media cache cleanup.
    async fn last_media_cleanup_time_inner(&self) -> Result<Option<SystemTime>, Self::Error>;
}

#[repr(transparent)]
struct EraseMediaStoreError<T>(T);

#[cfg(not(tarpaulin_include))]
impl<T: fmt::Debug> fmt::Debug for EraseMediaStoreError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl<T: MediaStore> MediaStore for EraseMediaStoreError<T> {
    type Error = MediaStoreError;

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<CrossProcessLockGeneration>, Self::Error> {
        self.0.try_take_leased_lock(lease_duration_ms, key, holder).await.map_err(Into::into)
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

    async fn clean(&self) -> Result<(), Self::Error> {
        self.0.clean().await.map_err(Into::into)
    }

    async fn optimize(&self) -> Result<(), Self::Error> {
        self.0.optimize().await.map_err(Into::into)
    }

    async fn get_size(&self) -> Result<Option<usize>, Self::Error> {
        self.0.get_size().await.map_err(Into::into)
    }
}

/// A type-erased [`MediaStore`].
pub type DynMediaStore = dyn MediaStore<Error = MediaStoreError>;

/// A type that can be type-erased into `Arc<dyn MediaStore>`.
///
/// This trait is not meant to be implemented directly outside
/// `matrix-sdk-base`, but it is automatically implemented for everything that
/// implements `MediaStore`.
pub trait IntoMediaStore {
    #[doc(hidden)]
    fn into_media_store(self) -> Arc<DynMediaStore>;
}

impl IntoMediaStore for Arc<DynMediaStore> {
    fn into_media_store(self) -> Arc<DynMediaStore> {
        self
    }
}

impl<T> IntoMediaStore for T
where
    T: MediaStore + Sized + 'static,
{
    fn into_media_store(self) -> Arc<DynMediaStore> {
        Arc::new(EraseMediaStoreError(self))
    }
}

// Turns a given `Arc<T>` into `Arc<DynMediaStore>` by attaching the
// `MediaStore` impl vtable of `EraseMediaStoreError<T>`.
impl<T> IntoMediaStore for Arc<T>
where
    T: MediaStore + 'static,
{
    fn into_media_store(self) -> Arc<DynMediaStore> {
        let ptr: *const T = Arc::into_raw(self);
        let ptr_erased = ptr as *const EraseMediaStoreError<T>;
        // SAFETY: EraseMediaStoreError is repr(transparent) so T and
        //         EraseMediaStoreError<T> have the same layout and ABI
        unsafe { Arc::from_raw(ptr_erased) }
    }
}
