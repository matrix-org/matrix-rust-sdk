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

//! Trait and macro of integration tests for `EventCacheStore` implementations.

use std::time::Duration;

use async_trait::async_trait;
use ruma::{
    api::client::media::get_content_thumbnail::v3::Method, events::room::MediaSource, mxc_uri,
    owned_mxc_uri, time::SystemTime, uint,
};

use super::{DynEventCacheStore, MediaRetentionPolicy};
use crate::media::{MediaFormat, MediaRequest, MediaThumbnailSettings};

/// `EventCacheStore` integration tests.
///
/// This trait is not meant to be used directly, but will be used with the
/// [`event_cache_store_integration_tests!`] macro.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait EventCacheStoreIntegrationTests {
    /// Test media content storage.
    async fn test_media_content(&self);

    /// Test media retention policy storage.
    async fn test_store_media_retention_policy(&self);

    /// Test media content's retention policy max file size.
    async fn test_media_max_file_size(&self);

    /// Test media content's retention policy max file size.
    async fn test_media_max_cache_size(&self);

    /// Test media content's retention policy expiry.
    async fn test_media_expiry(&self);
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl EventCacheStoreIntegrationTests for DynEventCacheStore {
    async fn test_media_content(&self) {
        let uri = mxc_uri!("mxc://localhost/media");
        let request_file =
            MediaRequest { source: MediaSource::Plain(uri.to_owned()), format: MediaFormat::File };
        let request_thumbnail = MediaRequest {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(
                Method::Crop,
                uint!(100),
                uint!(100),
            )),
        };

        let other_uri = owned_mxc_uri!("mxc://localhost/media-other");
        let request_other_file =
            MediaRequest { source: MediaSource::Plain(other_uri), format: MediaFormat::File };

        let content: Vec<u8> = "hello".into();
        let thumbnail_content: Vec<u8> = "world".into();
        let other_content: Vec<u8> = "foo".into();
        let time = SystemTime::now();
        let policy = MediaRetentionPolicy::empty();

        // Media isn't present in the cache.
        assert!(
            self.get_media_content(&request_file, time).await.unwrap().is_none(),
            "unexpected media found"
        );
        assert!(
            self.get_media_content(&request_thumbnail, time).await.unwrap().is_none(),
            "media not found"
        );

        // Let's add the media.
        self.add_media_content(&request_file, content.clone(), time, policy)
            .await
            .expect("adding media failed");

        // Media is present in the cache.
        assert_eq!(
            self.get_media_content(&request_file, time).await.unwrap().as_ref(),
            Some(&content),
            "media not found though added"
        );

        // Let's remove the media.
        self.remove_media_content(&request_file).await.expect("removing media failed");

        // Media isn't present in the cache.
        assert!(
            self.get_media_content(&request_file, time).await.unwrap().is_none(),
            "media still there after removing"
        );

        // Let's add the media again.
        self.add_media_content(&request_file, content.clone(), time, policy)
            .await
            .expect("adding media again failed");

        assert_eq!(
            self.get_media_content(&request_file, time).await.unwrap().as_ref(),
            Some(&content),
            "media not found after adding again"
        );

        // Let's add the thumbnail media.
        self.add_media_content(&request_thumbnail, thumbnail_content.clone(), time, policy)
            .await
            .expect("adding thumbnail failed");

        // Media's thumbnail is present.
        assert_eq!(
            self.get_media_content(&request_thumbnail, time).await.unwrap().as_ref(),
            Some(&thumbnail_content),
            "thumbnail not found"
        );

        // Let's add another media with a different URI.
        self.add_media_content(&request_other_file, other_content.clone(), time, policy)
            .await
            .expect("adding other media failed");

        // Other file is present.
        assert_eq!(
            self.get_media_content(&request_other_file, time).await.unwrap().as_ref(),
            Some(&other_content),
            "other file not found"
        );

        // Let's remove media based on URI.
        self.remove_media_content_for_uri(uri).await.expect("removing all media for uri failed");

        assert!(
            self.get_media_content(&request_file, time).await.unwrap().is_none(),
            "media wasn't removed"
        );
        assert!(
            self.get_media_content(&request_thumbnail, time).await.unwrap().is_none(),
            "thumbnail wasn't removed"
        );
        assert!(
            self.get_media_content(&request_other_file, time).await.unwrap().is_some(),
            "other media was removed"
        );
    }

    async fn test_store_media_retention_policy(&self) {
        let stored = self.media_retention_policy().await.unwrap();
        assert!(stored.is_none());

        let policy = MediaRetentionPolicy::default();
        self.set_media_retention_policy(policy).await.unwrap();

        let stored = self.media_retention_policy().await.unwrap();
        assert_eq!(stored, Some(policy));
    }

    async fn test_media_max_file_size(&self) {
        let time = SystemTime::now();

        // 256 bytes content.
        let content_big = vec![0; 256];
        let uri_big = owned_mxc_uri!("mxc://localhost/big-media");
        let request_big =
            MediaRequest { source: MediaSource::Plain(uri_big), format: MediaFormat::File };

        // 128 bytes content.
        let content_avg = vec![0; 128];
        let uri_avg = owned_mxc_uri!("mxc://localhost/average-media");
        let request_avg =
            MediaRequest { source: MediaSource::Plain(uri_avg), format: MediaFormat::File };

        // 64 bytes content.
        let content_small = vec![0; 64];
        let uri_small = owned_mxc_uri!("mxc://localhost/small-media");
        let request_small =
            MediaRequest { source: MediaSource::Plain(uri_small), format: MediaFormat::File };

        // First, with a policy that doesn't accept the big media.
        let policy = MediaRetentionPolicy::empty().with_max_file_size(Some(200));

        self.add_media_content(&request_big, content_big.clone(), time, policy).await.unwrap();
        self.add_media_content(&request_avg, content_avg.clone(), time, policy).await.unwrap();
        self.add_media_content(&request_small, content_small, time, policy).await.unwrap();

        // The big content was NOT cached but the others were.
        let stored = self.get_media_content(&request_big, time).await.unwrap();
        assert!(stored.is_none());
        let stored = self.get_media_content(&request_avg, time).await.unwrap();
        assert!(stored.is_some());
        let stored = self.get_media_content(&request_small, time).await.unwrap();
        assert!(stored.is_some());

        // A cleanup doesn't have any effect.
        self.clean_up_media_cache(policy, time).await.unwrap();

        let stored = self.get_media_content(&request_avg, time).await.unwrap();
        assert!(stored.is_some());
        let stored = self.get_media_content(&request_small, time).await.unwrap();
        assert!(stored.is_some());

        // Change to a policy that doesn't accept the average media.
        let policy = MediaRetentionPolicy::empty().with_max_file_size(Some(100));

        // The cleanup removes the average media.
        self.clean_up_media_cache(policy, time).await.unwrap();

        let stored = self.get_media_content(&request_avg, time).await.unwrap();
        assert!(stored.is_none());
        let stored = self.get_media_content(&request_small, time).await.unwrap();
        assert!(stored.is_some());

        // Caching big and average media doesn't work.
        self.add_media_content(&request_big, content_big.clone(), time, policy).await.unwrap();
        self.add_media_content(&request_avg, content_avg.clone(), time, policy).await.unwrap();

        let stored = self.get_media_content(&request_big, time).await.unwrap();
        assert!(stored.is_none());
        let stored = self.get_media_content(&request_avg, time).await.unwrap();
        assert!(stored.is_none());

        // If there are both a cache size and a file size, the minimum value is used.
        let policy = MediaRetentionPolicy::empty()
            .with_max_cache_size(Some(200))
            .with_max_file_size(Some(1000));

        // Caching big doesn't work.
        self.add_media_content(&request_big, content_big.clone(), time, policy).await.unwrap();
        self.add_media_content(&request_avg, content_avg.clone(), time, policy).await.unwrap();

        let stored = self.get_media_content(&request_big, time).await.unwrap();
        assert!(stored.is_none());
        let stored = self.get_media_content(&request_avg, time).await.unwrap();
        assert!(stored.is_some());

        // Change to a policy that doesn't accept the average media.
        let policy = MediaRetentionPolicy::empty()
            .with_max_cache_size(Some(100))
            .with_max_file_size(Some(1000));

        // The cleanup removes the average media.
        self.clean_up_media_cache(policy, time).await.unwrap();

        let stored = self.get_media_content(&request_avg, time).await.unwrap();
        assert!(stored.is_none());
        let stored = self.get_media_content(&request_small, time).await.unwrap();
        assert!(stored.is_some());

        // Caching big and average media doesn't work.
        self.add_media_content(&request_big, content_big, time, policy).await.unwrap();
        self.add_media_content(&request_avg, content_avg, time, policy).await.unwrap();

        let stored = self.get_media_content(&request_big, time).await.unwrap();
        assert!(stored.is_none());
        let stored = self.get_media_content(&request_avg, time).await.unwrap();
        assert!(stored.is_none());
    }

    async fn test_media_max_cache_size(&self) {
        // 256 bytes content.
        let content_big = vec![0; 256];
        let uri_big = owned_mxc_uri!("mxc://localhost/big-media");
        let request_big =
            MediaRequest { source: MediaSource::Plain(uri_big), format: MediaFormat::File };

        // 128 bytes content.
        let content_avg = vec![0; 128];
        let uri_avg = owned_mxc_uri!("mxc://localhost/average-media");
        let request_avg =
            MediaRequest { source: MediaSource::Plain(uri_avg), format: MediaFormat::File };

        // 64 bytes content.
        let content_small = vec![0; 64];
        let uri_small_1 = owned_mxc_uri!("mxc://localhost/small-media-1");
        let request_small_1 =
            MediaRequest { source: MediaSource::Plain(uri_small_1), format: MediaFormat::File };
        let uri_small_2 = owned_mxc_uri!("mxc://localhost/small-media-2");
        let request_small_2 =
            MediaRequest { source: MediaSource::Plain(uri_small_2), format: MediaFormat::File };
        let uri_small_3 = owned_mxc_uri!("mxc://localhost/small-media-3");
        let request_small_3 =
            MediaRequest { source: MediaSource::Plain(uri_small_3), format: MediaFormat::File };
        let uri_small_4 = owned_mxc_uri!("mxc://localhost/small-media-4");
        let request_small_4 =
            MediaRequest { source: MediaSource::Plain(uri_small_4), format: MediaFormat::File };
        let uri_small_5 = owned_mxc_uri!("mxc://localhost/small-media-5");
        let request_small_5 =
            MediaRequest { source: MediaSource::Plain(uri_small_5), format: MediaFormat::File };

        // A policy that doesn't accept the big media.
        let policy = MediaRetentionPolicy::empty().with_max_cache_size(Some(200));

        // Try to add all the content at different times.
        let mut time = SystemTime::UNIX_EPOCH;
        self.add_media_content(&request_big, content_big, time, policy).await.unwrap();
        time += Duration::from_secs(1);
        self.add_media_content(&request_small_1, content_small.clone(), time, policy)
            .await
            .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content(&request_small_2, content_small.clone(), time, policy)
            .await
            .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content(&request_small_3, content_small.clone(), time, policy)
            .await
            .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content(&request_small_4, content_small.clone(), time, policy)
            .await
            .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content(&request_small_5, content_small.clone(), time, policy)
            .await
            .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content(&request_avg, content_avg, time, policy).await.unwrap();

        // The big content was NOT cached but the others were.
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_big, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_1, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_2, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_3, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_4, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_5, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_avg, time).await.unwrap();
        assert!(stored.is_some());

        // Cleanup removes the oldest content first.
        time += Duration::from_secs(1);
        self.clean_up_media_cache(policy, time).await.unwrap();

        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_1, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_2, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_3, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_4, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_5, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_avg, time).await.unwrap();
        assert!(stored.is_some());

        // Reinsert the small medias that were removed.
        time += Duration::from_secs(1);
        self.add_media_content(&request_small_1, content_small.clone(), time, policy)
            .await
            .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content(&request_small_2, content_small.clone(), time, policy)
            .await
            .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content(&request_small_3, content_small.clone(), time, policy)
            .await
            .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content(&request_small_4, content_small, time, policy).await.unwrap();

        // Check that they are cached.
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_1, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_2, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_3, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_4, time).await.unwrap();
        assert!(stored.is_some());

        // Access small_5 too so its last access is updated too.
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_5, time).await.unwrap();
        assert!(stored.is_some());

        // Cleanup still removes the oldest content first, which is not the same as
        // before.
        time += Duration::from_secs(1);
        self.clean_up_media_cache(policy, time).await.unwrap();

        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_1, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_2, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_3, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_4, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_small_5, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_avg, time).await.unwrap();
        assert!(stored.is_none());
    }

    async fn test_media_expiry(&self) {
        // 64 bytes content.
        let content = vec![0; 64];

        let uri_1 = owned_mxc_uri!("mxc://localhost/media-1");
        let request_1 =
            MediaRequest { source: MediaSource::Plain(uri_1), format: MediaFormat::File };
        let uri_2 = owned_mxc_uri!("mxc://localhost/media-2");
        let request_2 =
            MediaRequest { source: MediaSource::Plain(uri_2), format: MediaFormat::File };
        let uri_3 = owned_mxc_uri!("mxc://localhost/media-3");
        let request_3 =
            MediaRequest { source: MediaSource::Plain(uri_3), format: MediaFormat::File };
        let uri_4 = owned_mxc_uri!("mxc://localhost/media-4");
        let request_4 =
            MediaRequest { source: MediaSource::Plain(uri_4), format: MediaFormat::File };
        let uri_5 = owned_mxc_uri!("mxc://localhost/media-5");
        let request_5 =
            MediaRequest { source: MediaSource::Plain(uri_5), format: MediaFormat::File };

        // A policy with 30 seconds expiry.
        let policy =
            MediaRetentionPolicy::empty().with_last_access_expiry(Some(Duration::from_secs(30)));

        // Add all the content at different times.
        let mut time = SystemTime::UNIX_EPOCH;
        self.add_media_content(&request_1, content.clone(), time, policy).await.unwrap();
        time += Duration::from_secs(1);
        self.add_media_content(&request_2, content.clone(), time, policy).await.unwrap();
        time += Duration::from_secs(1);
        self.add_media_content(&request_3, content.clone(), time, policy).await.unwrap();
        time += Duration::from_secs(1);
        self.add_media_content(&request_4, content.clone(), time, policy).await.unwrap();
        time += Duration::from_secs(1);
        self.add_media_content(&request_5, content, time, policy).await.unwrap();

        // The content was cached.
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_1, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_2, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_3, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_4, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_5, time).await.unwrap();
        assert!(stored.is_some());

        // We are now at UNIX_EPOCH + 10 seconds, the oldest content was accessed 5
        // seconds ago.
        time += Duration::from_secs(1);
        assert_eq!(time, SystemTime::UNIX_EPOCH + Duration::from_secs(10));

        // Cleanup has no effect, nothing has expired.
        self.clean_up_media_cache(policy, time).await.unwrap();

        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_1, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_2, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_3, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_4, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_5, time).await.unwrap();
        assert!(stored.is_some());

        // We are now at UNIX_EPOCH + 16 seconds, the oldest content was accessed 5
        // seconds ago.
        time += Duration::from_secs(1);
        assert_eq!(time, SystemTime::UNIX_EPOCH + Duration::from_secs(16));

        // Jump 26 seconds in the future, so the 2 first media contents are expired.
        time += Duration::from_secs(26);

        // Cleanup removes the two oldest media contents.
        self.clean_up_media_cache(policy, time).await.unwrap();

        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_1, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_2, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_3, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_4, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content(&request_5, time).await.unwrap();
        assert!(stored.is_some());
    }
}

/// Macro building to allow your `EventCacheStore` implementation to run the
/// entire tests suite locally.
///
/// Can be run with the `with_media_size_tests` argument to include more tests
/// about the media cache retention policy based on content size. It is not
/// recommended to run those in encrypted stores because the size of the
/// encrypted content may vary compared to what the tests expect.
///
/// You need to provide a `async fn get_event_cache_store() ->
/// EventCacheStoreResult<impl EventCacheStore>` providing a fresh event cache
/// store on the same level you invoke the macro.
///
/// ## Usage Example:
/// ```no_run
/// # use matrix_sdk_base::event_cache_store::{
/// #    EventCacheStore,
/// #    MemoryStore as MyStore,
/// #    Result as EventCacheStoreResult,
/// # };
///
/// #[cfg(test)]
/// mod tests {
///     use super::{EventCacheStore, EventCacheStoreResult, MyStore};
///
///     async fn get_event_cache_store(
///     ) -> EventCacheStoreResult<impl EventCacheStore> {
///         Ok(MyStore::new())
///     }
///
///     event_cache_store_integration_tests!();
/// }
/// ```
#[allow(unused_macros, unused_extern_crates)]
#[macro_export]
macro_rules! event_cache_store_integration_tests {
    (with_media_size_tests) => {
        mod event_cache_store_integration_tests {
            $crate::event_cache_store_integration_tests!(@inner);

            #[async_test]
            async fn test_media_max_file_size() {
                let event_cache_store = get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_media_max_file_size().await;
            }

            #[async_test]
            async fn test_media_max_cache_size() {
                let event_cache_store = get_event_cache_store().await.unwrap().into_event_cache_store();
                event_cache_store.test_media_max_cache_size().await;
            }
        }
    };

    () => {
        mod event_cache_store_integration_tests {
            $crate::event_cache_store_integration_tests!(@inner);
        }
    };

    (@inner) => {
        use matrix_sdk_test::async_test;
        use $crate::event_cache_store::{EventCacheStoreIntegrationTests, IntoEventCacheStore};

        use super::get_event_cache_store;

        #[async_test]
        async fn test_media_content() {
            let event_cache_store =
                get_event_cache_store().await.unwrap().into_event_cache_store();
            event_cache_store.test_media_content().await;
        }

        #[async_test]
        async fn test_store_media_retention_policy() {
            let event_cache_store = get_event_cache_store().await.unwrap().into_event_cache_store();
            event_cache_store.test_store_media_retention_policy().await;
        }

        #[async_test]
        async fn test_media_expiry() {
            let event_cache_store = get_event_cache_store().await.unwrap().into_event_cache_store();
            event_cache_store.test_media_expiry().await;
        }
    };
}
