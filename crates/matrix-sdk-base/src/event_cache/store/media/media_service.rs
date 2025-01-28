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

use std::fmt;

use async_trait::async_trait;
use matrix_sdk_common::{locks::Mutex, AsyncTraitDeps};
use ruma::{time::SystemTime, MxcUri};
use tokio::sync::Mutex as AsyncMutex;

use super::MediaRetentionPolicy;
use crate::{event_cache::store::EventCacheStoreError, media::MediaRequestParameters};

/// API for implementors of [`EventCacheStore`] to manage their media through
/// their implementation of [`EventCacheStoreMedia`].
///
/// [`EventCacheStore`]: crate::event_cache::store::EventCacheStore
#[derive(Debug)]
pub struct MediaService<Time: TimeProvider = DefaultTimeProvider> {
    /// The time provider.
    time_provider: Time,

    /// The current [`MediaRetentionPolicy`].
    policy: Mutex<MediaRetentionPolicy>,

    /// A mutex to ensure a single cleanup is running at a time.
    cleanup_guard: AsyncMutex<()>,
}

impl MediaService {
    /// Construct a new default `MediaService`.
    ///
    /// [`MediaService::restore()`] should be called after constructing the
    /// `MediaService` to restore its previous state.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for MediaService {
    fn default() -> Self {
        Self::with_time_provider(DefaultTimeProvider)
    }
}

impl<Time> MediaService<Time>
where
    Time: TimeProvider,
{
    /// Construct a new `MediaService` with the given `TimeProvider` and an
    /// empty `MediaRetentionPolicy`.
    fn with_time_provider(time_provider: Time) -> Self {
        Self {
            time_provider,
            policy: Mutex::new(MediaRetentionPolicy::empty()),
            cleanup_guard: AsyncMutex::new(()),
        }
    }

    /// Restore the previous state of the [`MediaRetentionPolicy`] from data
    /// that was persisted in the store.
    ///
    /// This should be called immediately after constructing the `MediaService`.
    ///
    /// # Arguments
    ///
    /// * `policy` - The `MediaRetentionPolicy` that was persisted in the store.
    pub fn restore(&self, policy: Option<MediaRetentionPolicy>) {
        if let Some(policy) = policy {
            *self.policy.lock() = policy;
        }
    }

    /// Set the `MediaRetentionPolicy` of this service.
    ///
    /// # Arguments
    ///
    /// * `store` - The `EventCacheStoreMedia`.
    ///
    /// * `policy` - The `MediaRetentionPolicy` to use.
    pub async fn set_media_retention_policy<Store: EventCacheStoreMedia>(
        &self,
        store: &Store,
        policy: MediaRetentionPolicy,
    ) -> Result<(), Store::Error> {
        store.set_media_retention_policy_inner(policy).await?;

        *self.policy.lock() = policy;

        Ok(())
    }

    /// Get the `MediaRetentionPolicy` of this service.
    pub fn media_retention_policy(&self) -> MediaRetentionPolicy {
        *self.policy.lock()
    }

    /// Add a media file's content in the media store.
    ///
    /// # Arguments
    ///
    /// * `store` - The `EventCacheStoreMedia`.
    ///
    /// * `request` - The `MediaRequestParameters` of the file.
    ///
    /// * `content` - The content of the file.
    ///
    /// * `ignore_policy` - Whether the current `MediaRetentionPolicy` should be
    ///   ignored.
    pub async fn add_media_content<Store: EventCacheStoreMedia>(
        &self,
        store: &Store,
        request: &MediaRequestParameters,
        content: Vec<u8>,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Store::Error> {
        let policy = self.media_retention_policy();

        if ignore_policy == IgnoreMediaRetentionPolicy::No
            && policy.exceeds_max_file_size(content.len())
        {
            // We do not cache the content.
            return Ok(());
        }

        store
            .add_media_content_inner(
                request,
                content,
                self.time_provider.now(),
                policy,
                ignore_policy,
            )
            .await
    }

    /// Set whether the current [`MediaRetentionPolicy`] should be ignored for
    /// the media.
    ///
    /// The change will be taken into account in the next cleanup.
    ///
    /// # Arguments
    ///
    /// * `store` - The `EventCacheStoreMedia`.
    ///
    /// * `request` - The `MediaRequestParameters` of the file.
    ///
    /// * `ignore_policy` - Whether the current `MediaRetentionPolicy` should be
    ///   ignored.
    pub async fn set_ignore_media_retention_policy<Store: EventCacheStoreMedia>(
        &self,
        store: &Store,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Store::Error> {
        store.set_ignore_media_retention_policy_inner(request, ignore_policy).await
    }

    /// Get a media file's content out of the media store.
    ///
    /// # Arguments
    ///
    /// * `store` - The `EventCacheStoreMedia`.
    ///
    /// * `request` - The `MediaRequestParameters` of the file.
    pub async fn get_media_content<Store: EventCacheStoreMedia>(
        &self,
        store: &Store,
        request: &MediaRequestParameters,
    ) -> Result<Option<Vec<u8>>, Store::Error> {
        store.get_media_content_inner(request, self.time_provider.now()).await
    }

    /// Get a media file's content associated to an `MxcUri` from the
    /// media store.
    ///
    /// # Arguments
    ///
    /// * `store` - The `EventCacheStoreMedia`.
    ///
    /// * `uri` - The `MxcUri` of the media file.
    pub async fn get_media_content_for_uri<Store: EventCacheStoreMedia>(
        &self,
        store: &Store,
        uri: &MxcUri,
    ) -> Result<Option<Vec<u8>>, Store::Error> {
        store.get_media_content_for_uri_inner(uri, self.time_provider.now()).await
    }

    /// Clean up the media cache with the current `MediaRetentionPolicy`.
    ///
    /// If there is already an ongoing cleanup, this is a noop.
    ///
    /// # Arguments
    ///
    /// * `store` - The `EventCacheStoreMedia`.
    pub async fn clean_up_media_cache<Store: EventCacheStoreMedia>(
        &self,
        store: &Store,
    ) -> Result<(), Store::Error> {
        let Ok(_guard) = self.cleanup_guard.try_lock() else {
            // There is another ongoing cleanup.
            return Ok(());
        };

        let policy = self.media_retention_policy();

        if !policy.has_limitations() {
            // No need to call the backend.
            return Ok(());
        }

        store.clean_up_media_cache_inner(policy, self.time_provider.now()).await
    }
}

/// An abstract trait that can be used to implement different store backends
/// for the media cache of the SDK.
///
/// The main purposes of this trait are to be able to centralize where we handle
/// [`MediaRetentionPolicy`] by wrapping this in a [`MediaService`], and to
/// simplify the implementation of tests by being able to have complete control
/// over the `SystemTime`s provided to the store.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait EventCacheStoreMedia: AsyncTraitDeps {
    /// The error type used by this media cache store.
    type Error: fmt::Debug + Into<EventCacheStoreError>;

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
    ///   content.
    async fn clean_up_media_cache_inner(
        &self,
        policy: MediaRetentionPolicy,
        current_time: SystemTime,
    ) -> Result<(), Self::Error>;
}

/// Whether the [`MediaRetentionPolicy`] should be ignored for the current
/// content.
///
/// Some media cache actions are noops when the media content that is processed
/// is filtered out by the policy. This can break some features of the SDK, like
/// the send queue, that expects to be able to persist all media files in the
/// store to restore them when the client is restored.
///
/// This can be converted to a boolean with
/// [`IgnoreMediaRetentionPolicy::is_yes()`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreMediaRetentionPolicy {
    /// The media retention policy will be ignored and the current action will
    /// not be a noop.
    ///
    /// Any media content in this state must NOT be used when applying a
    /// `MediaRetentionPolicy`. This applies to ANY criteria, like the maximum
    /// file size, the maximum cache size or the last access expiry.
    ///
    /// This state is supposed to be transient, and to only be used internally
    /// by the SDK.
    Yes,

    /// The media retention policy will be respected and the current action
    /// might be a noop.
    No,
}

impl IgnoreMediaRetentionPolicy {
    /// Whether this is an [`IgnoreMediaRetentionPolicy::Yes`] variant.
    pub fn is_yes(self) -> bool {
        matches!(self, Self::Yes)
    }
}

/// An abstract trait to provide the current `SystemTime` for the
/// [`MediaService`].
pub trait TimeProvider {
    /// The current time.
    fn now(&self) -> SystemTime;
}

/// The default time provider, that calls `ruma::time::SystemTime::now()`.
#[derive(Debug)]
pub struct DefaultTimeProvider;

impl TimeProvider for DefaultTimeProvider {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

#[cfg(test)]
mod tests {
    use std::{fmt, sync::MutexGuard};

    use async_trait::async_trait;
    use matrix_sdk_common::locks::Mutex;
    use matrix_sdk_test::async_test;
    use ruma::{
        events::room::MediaSource,
        mxc_uri,
        time::{Duration, SystemTime},
        MxcUri, OwnedMxcUri,
    };

    use super::{EventCacheStoreMedia, IgnoreMediaRetentionPolicy, MediaService, TimeProvider};
    use crate::{
        event_cache::store::{media::MediaRetentionPolicy, EventCacheStoreError},
        media::{MediaFormat, MediaRequestParameters, UniqueKey},
    };

    #[derive(Debug, Default)]
    struct MockEventCacheStoreMedia {
        inner: Mutex<MockEventCacheStoreMediaInner>,
    }

    impl MockEventCacheStoreMedia {
        /// Whether the store was accessed.
        fn accessed(&self) -> bool {
            self.inner.lock().accessed
        }

        /// Reset the `accessed` boolean.
        fn reset_accessed(&self) {
            self.inner.lock().accessed = false;
        }

        /// Access the inner store.
        ///
        /// Should be called for every access to the inner store as it also sets
        /// the `accessed` boolean.
        fn inner(&self) -> MutexGuard<'_, MockEventCacheStoreMediaInner> {
            let mut inner = self.inner.lock();
            inner.accessed = true;
            inner
        }
    }

    #[derive(Debug, Default)]
    struct MockEventCacheStoreMediaInner {
        /// Whether this store was accessed.
        ///
        /// Must be set to `true` for any operation that unlocks the store.
        accessed: bool,

        /// The persisted media retention policy.
        media_retention_policy: Option<MediaRetentionPolicy>,

        /// The list of media content.
        media_list: Vec<MediaContent>,

        /// The time of the last cleanup.
        cleanup_time: Option<SystemTime>,
    }

    #[derive(Debug, Clone)]
    struct MediaContent {
        /// The unique key for the media content.
        key: String,

        /// The original URI of the media content.
        uri: OwnedMxcUri,

        /// The media content.
        content: Vec<u8>,

        /// Whether the `MediaRetentionPolicy` should be ignored for this media
        /// content;
        ignore_policy: bool,

        /// The time of the last access of the media content.
        last_access: SystemTime,
    }

    #[derive(Debug)]
    struct MockEventCacheStoreMediaError;

    impl fmt::Display for MockEventCacheStoreMediaError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "MockEventCacheStoreMediaError")
        }
    }

    impl std::error::Error for MockEventCacheStoreMediaError {}

    impl From<MockEventCacheStoreMediaError> for EventCacheStoreError {
        fn from(value: MockEventCacheStoreMediaError) -> Self {
            Self::backend(value)
        }
    }

    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    impl EventCacheStoreMedia for MockEventCacheStoreMedia {
        type Error = MockEventCacheStoreMediaError;

        async fn media_retention_policy_inner(
            &self,
        ) -> Result<Option<MediaRetentionPolicy>, Self::Error> {
            Ok(self.inner().media_retention_policy)
        }

        async fn set_media_retention_policy_inner(
            &self,
            policy: MediaRetentionPolicy,
        ) -> Result<(), Self::Error> {
            self.inner().media_retention_policy = Some(policy);
            Ok(())
        }

        async fn add_media_content_inner(
            &self,
            request: &MediaRequestParameters,
            content: Vec<u8>,
            current_time: SystemTime,
            policy: MediaRetentionPolicy,
            ignore_policy: IgnoreMediaRetentionPolicy,
        ) -> Result<(), Self::Error> {
            let ignore_policy = ignore_policy.is_yes();

            if !ignore_policy && policy.exceeds_max_file_size(content.len()) {
                return Ok(());
            }

            let mut inner = self.inner();
            let key = request.unique_key();

            if let Some(pos) = inner.media_list.iter().position(|content| content.key == key) {
                let media_content = &mut inner.media_list[pos];
                media_content.content = content;
                media_content.last_access = current_time;
                media_content.ignore_policy = ignore_policy;
            } else {
                inner.media_list.push(MediaContent {
                    key,
                    uri: request.uri().to_owned(),
                    content,
                    ignore_policy,
                    last_access: current_time,
                });
            }

            Ok(())
        }

        async fn set_ignore_media_retention_policy_inner(
            &self,
            request: &MediaRequestParameters,
            ignore_policy: IgnoreMediaRetentionPolicy,
        ) -> Result<(), Self::Error> {
            let key = request.unique_key();
            let mut inner = self.inner();

            if let Some(pos) = inner.media_list.iter().position(|content| content.key == key) {
                inner.media_list[pos].ignore_policy = ignore_policy.is_yes();
            }

            Ok(())
        }

        async fn get_media_content_inner(
            &self,
            request: &MediaRequestParameters,
            current_time: SystemTime,
        ) -> Result<Option<Vec<u8>>, Self::Error> {
            let key = request.unique_key();
            let mut inner = self.inner();

            let Some(media_content) =
                inner.media_list.iter_mut().find(|content| content.key == key)
            else {
                return Ok(None);
            };

            media_content.last_access = current_time;

            Ok(Some(media_content.content.clone()))
        }

        async fn get_media_content_for_uri_inner(
            &self,
            uri: &MxcUri,
            current_time: SystemTime,
        ) -> Result<Option<Vec<u8>>, Self::Error> {
            let mut inner = self.inner();

            let Some(media_content) =
                inner.media_list.iter_mut().find(|content| content.uri == uri)
            else {
                return Ok(None);
            };

            media_content.last_access = current_time;

            Ok(Some(media_content.content.clone()))
        }

        async fn clean_up_media_cache_inner(
            &self,
            _policy: MediaRetentionPolicy,
            current_time: SystemTime,
        ) -> Result<(), Self::Error> {
            // This is mostly a noop. We don't care about this test implementation, only
            // whether this method was called with the right time.
            self.inner().cleanup_time = Some(current_time);

            Ok(())
        }
    }

    #[derive(Debug)]
    struct MockTimeProvider {
        now: Mutex<SystemTime>,
    }

    impl MockTimeProvider {
        /// Construct a `MockTimeProvider` with the given current time.
        fn new(now: SystemTime) -> Self {
            Self { now: Mutex::new(now) }
        }

        /// Set the current time.
        fn set_now(&self, now: SystemTime) {
            *self.now.lock() = now;
        }
    }

    impl TimeProvider for MockTimeProvider {
        fn now(&self) -> SystemTime {
            *self.now.lock()
        }
    }

    #[async_test]
    async fn test_media_service_empty_policy() {
        let content = b"some text content";
        let uri = mxc_uri!("mxc://server.local/AbcDe1234");
        let request = MediaRequestParameters {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::File,
        };

        let now = SystemTime::UNIX_EPOCH;

        let store = MockEventCacheStoreMedia::default();
        let service = MediaService::with_time_provider(MockTimeProvider::new(now));

        // By default an empty policy is used.
        assert!(!service.media_retention_policy().has_limitations());
        service.restore(None);
        assert!(!service.media_retention_policy().has_limitations());
        assert!(!store.accessed());

        // Add media.
        service
            .add_media_content(&store, &request, content.to_vec(), IgnoreMediaRetentionPolicy::No)
            .await
            .unwrap();
        assert!(store.accessed());

        let media_content = store.inner().media_list[0].clone();
        assert_eq!(media_content.uri, uri);
        assert_eq!(media_content.content, content);
        assert!(!media_content.ignore_policy);
        assert_eq!(media_content.last_access, now);

        let now = now + Duration::from_secs(60);
        service.time_provider.set_now(now);
        store.reset_accessed();

        // Get media from request.
        let loaded_content = service.get_media_content(&store, &request).await.unwrap();
        assert!(store.accessed());
        assert_eq!(loaded_content.as_deref(), Some(content.as_slice()));

        // The last access time was updated.
        let media = store.inner().media_list[0].clone();
        assert_eq!(media.last_access, now);

        let now = now + Duration::from_secs(60);
        service.time_provider.set_now(now);
        store.reset_accessed();

        // Get media from URI.
        let loaded_content = service.get_media_content_for_uri(&store, uri).await.unwrap();
        assert!(store.accessed());
        assert_eq!(loaded_content.as_deref(), Some(content.as_slice()));

        // The last access time was updated.
        let media = store.inner().media_list[0].clone();
        assert_eq!(media.last_access, now);

        // Update ignore_policy.
        service
            .set_ignore_media_retention_policy(&store, &request, IgnoreMediaRetentionPolicy::Yes)
            .await
            .unwrap();
        assert!(store.accessed());

        let media_content = store.inner().media_list[0].clone();
        assert!(media_content.ignore_policy);

        // Try a cleanup. With the empty policy the store should not be accessed.
        assert_eq!(store.inner().cleanup_time, None);
        store.reset_accessed();

        service.clean_up_media_cache(&store).await.unwrap();
        assert!(!store.accessed());
        assert_eq!(store.inner().cleanup_time, None);
    }

    #[async_test]
    async fn test_media_service_non_empty_policy() {
        // Content of less than 32 bytes.
        let small_content = b"some text content";
        let small_uri = mxc_uri!("mxc://server.local/small");
        let small_request = MediaRequestParameters {
            source: MediaSource::Plain(small_uri.to_owned()),
            format: MediaFormat::File,
        };

        // Content of more than 32 bytes.
        let big_content = b"some much much larger text content";
        let big_uri = mxc_uri!("mxc://server.local/big");
        let big_request = MediaRequestParameters {
            source: MediaSource::Plain(big_uri.to_owned()),
            format: MediaFormat::File,
        };

        // Limit the file size to 32 bytes in the retention policy.
        let policy = MediaRetentionPolicy { max_file_size: Some(32), ..Default::default() };

        let now = SystemTime::UNIX_EPOCH;

        let store = MockEventCacheStoreMedia::default();
        let service = MediaService::with_time_provider(MockTimeProvider::new(now));

        // Check that restoring the policy works.
        service.restore(Some(MediaRetentionPolicy::default()));
        assert_eq!(service.media_retention_policy(), MediaRetentionPolicy::default());
        assert!(!store.accessed());

        // Set the media retention policy.
        service.set_media_retention_policy(&store, policy).await.unwrap();
        assert!(store.accessed());
        assert_eq!(service.media_retention_policy(), policy);
        assert_eq!(store.inner().media_retention_policy, Some(policy));

        store.reset_accessed();

        // Add small media, it should work because its size is lower than the max file
        // size.
        service
            .add_media_content(
                &store,
                &small_request,
                small_content.to_vec(),
                IgnoreMediaRetentionPolicy::No,
            )
            .await
            .unwrap();
        assert!(store.accessed());

        let media_content = store.inner().media_list[0].clone();
        assert_eq!(media_content.uri, small_uri);
        assert_eq!(media_content.content, small_content);
        assert!(!media_content.ignore_policy);
        assert_eq!(media_content.last_access, now);

        let now = now + Duration::from_secs(60);
        service.time_provider.set_now(now);
        store.reset_accessed();

        // Get media from request.
        let loaded_content = service.get_media_content(&store, &small_request).await.unwrap();
        assert!(store.accessed());
        assert_eq!(loaded_content.as_deref(), Some(small_content.as_slice()));

        // The last access time was updated.
        let media = store.inner().media_list[0].clone();
        assert_eq!(media.last_access, now);

        let now = now + Duration::from_secs(60);
        service.time_provider.set_now(now);
        store.reset_accessed();

        // Get media from URI.
        let loaded_content = service.get_media_content_for_uri(&store, small_uri).await.unwrap();
        assert!(store.accessed());
        assert_eq!(loaded_content.as_deref(), Some(small_content.as_slice()));

        // The last access time was updated.
        let media = store.inner().media_list[0].clone();
        assert_eq!(media.last_access, now);

        let now = now + Duration::from_secs(60);
        service.time_provider.set_now(now);
        store.reset_accessed();

        // Add big media, it will not work because it is bigger than the max file size.
        service
            .add_media_content(
                &store,
                &big_request,
                big_content.to_vec(),
                IgnoreMediaRetentionPolicy::No,
            )
            .await
            .unwrap();
        assert!(!store.accessed());
        assert_eq!(store.inner().media_list.len(), 1);

        store.reset_accessed();

        let loaded_content = service.get_media_content(&store, &big_request).await.unwrap();
        assert!(store.accessed());
        assert_eq!(loaded_content, None);

        store.reset_accessed();

        let loaded_content = service.get_media_content_for_uri(&store, big_uri).await.unwrap();
        assert!(store.accessed());
        assert_eq!(loaded_content, None);

        // Add big media, but this time ignore the policy.
        service
            .add_media_content(
                &store,
                &big_request,
                big_content.to_vec(),
                IgnoreMediaRetentionPolicy::Yes,
            )
            .await
            .unwrap();
        assert!(store.accessed());
        assert_eq!(store.inner().media_list.len(), 2);

        store.reset_accessed();

        // Get media from request.
        let loaded_content = service.get_media_content(&store, &big_request).await.unwrap();
        assert!(store.accessed());
        assert_eq!(loaded_content.as_deref(), Some(big_content.as_slice()));

        // The last access time was updated.
        let media = store.inner().media_list[1].clone();
        assert_eq!(media.last_access, now);

        let now = now + Duration::from_secs(60);
        service.time_provider.set_now(now);
        store.reset_accessed();

        // Get media from URI.
        let loaded_content = service.get_media_content_for_uri(&store, big_uri).await.unwrap();
        assert!(store.accessed());
        assert_eq!(loaded_content.as_deref(), Some(big_content.as_slice()));

        // The last access time was updated.
        let media = store.inner().media_list[1].clone();
        assert_eq!(media.last_access, now);

        // Try a cleanup, the store should be accessed.
        assert_eq!(store.inner().cleanup_time, None);

        let now = now + Duration::from_secs(60);
        service.time_provider.set_now(now);
        store.reset_accessed();

        service.clean_up_media_cache(&store).await.unwrap();
        assert!(store.accessed());
        assert_eq!(store.inner().cleanup_time, Some(now));
    }
}
