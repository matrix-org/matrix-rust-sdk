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

//! Trait and macro of integration tests for `EventCacheStoreMedia`
//! implementations.

use ruma::{
    events::room::MediaSource,
    mxc_uri, owned_mxc_uri,
    time::{Duration, SystemTime},
};

use super::{
    media_service::IgnoreMediaRetentionPolicy, EventCacheStoreMedia, MediaRetentionPolicy,
};
use crate::media::{MediaFormat, MediaRequestParameters};

/// [`EventCacheStoreMedia`] integration tests.
///
/// This trait is not meant to be used directly, but will be used with the
/// `event_cache_store_media_integration_tests!` macro.
#[allow(async_fn_in_trait)]
pub trait EventCacheStoreMediaIntegrationTests {
    /// Test media retention policy storage.
    async fn test_store_media_retention_policy(&self);

    /// Test media content's retention policy max file size.
    async fn test_media_max_file_size(&self);

    /// Test media content's retention policy max cache size.
    async fn test_media_max_cache_size(&self);

    /// Test media content's retention policy expiry.
    async fn test_media_expiry(&self);

    /// Test [`IgnoreMediaRetentionPolicy`] with the media content's retention
    /// policy max sizes.
    async fn test_media_ignore_max_size(&self);

    /// Test [`IgnoreMediaRetentionPolicy`] with the media content's retention
    /// policy expiry.
    async fn test_media_ignore_expiry(&self);

    /// Test last media cleanup time storage.
    async fn test_store_last_media_cleanup_time(&self);
}

impl<Store> EventCacheStoreMediaIntegrationTests for Store
where
    Store: EventCacheStoreMedia + std::fmt::Debug,
{
    async fn test_store_media_retention_policy(&self) {
        let stored = self.media_retention_policy_inner().await.unwrap();
        assert!(stored.is_none());

        let policy = MediaRetentionPolicy::default();
        self.set_media_retention_policy_inner(policy).await.unwrap();

        let stored = self.media_retention_policy_inner().await.unwrap();
        assert_eq!(stored, Some(policy));
    }

    async fn test_media_max_file_size(&self) {
        let time = SystemTime::now();

        // 256 bytes content.
        let content_big = vec![0; 256];
        let uri_big = owned_mxc_uri!("mxc://localhost/big-media");
        let request_big = MediaRequestParameters {
            source: MediaSource::Plain(uri_big),
            format: MediaFormat::File,
        };

        // 128 bytes content.
        let content_avg = vec![0; 128];
        let uri_avg = owned_mxc_uri!("mxc://localhost/average-media");
        let request_avg = MediaRequestParameters {
            source: MediaSource::Plain(uri_avg),
            format: MediaFormat::File,
        };

        // 64 bytes content.
        let content_small = vec![0; 64];
        let uri_small = owned_mxc_uri!("mxc://localhost/small-media");
        let request_small = MediaRequestParameters {
            source: MediaSource::Plain(uri_small),
            format: MediaFormat::File,
        };

        // First, with a policy that doesn't accept the big media.
        let policy = MediaRetentionPolicy::empty().with_max_file_size(Some(200));

        self.add_media_content_inner(
            &request_big,
            content_big.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        self.add_media_content_inner(
            &request_avg,
            content_avg.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        self.add_media_content_inner(
            &request_small,
            content_small,
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();

        // The big content was NOT cached but the others were.
        let stored = self.get_media_content_inner(&request_big, time).await.unwrap();
        assert!(stored.is_none());
        let stored = self.get_media_content_inner(&request_avg, time).await.unwrap();
        assert!(stored.is_some());
        let stored = self.get_media_content_inner(&request_small, time).await.unwrap();
        assert!(stored.is_some());

        // A cleanup doesn't have any effect.
        self.clean_up_media_cache_inner(policy, time).await.unwrap();

        let stored = self.get_media_content_inner(&request_avg, time).await.unwrap();
        assert!(stored.is_some());
        let stored = self.get_media_content_inner(&request_small, time).await.unwrap();
        assert!(stored.is_some());

        // Change to a policy that doesn't accept the average media.
        let policy = MediaRetentionPolicy::empty().with_max_file_size(Some(100));

        // The cleanup removes the average media.
        self.clean_up_media_cache_inner(policy, time).await.unwrap();

        let stored = self.get_media_content_inner(&request_avg, time).await.unwrap();
        assert!(stored.is_none());
        let stored = self.get_media_content_inner(&request_small, time).await.unwrap();
        assert!(stored.is_some());

        // Caching big and average media doesn't work.
        self.add_media_content_inner(
            &request_big,
            content_big.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        self.add_media_content_inner(
            &request_avg,
            content_avg.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();

        let stored = self.get_media_content_inner(&request_big, time).await.unwrap();
        assert!(stored.is_none());
        let stored = self.get_media_content_inner(&request_avg, time).await.unwrap();
        assert!(stored.is_none());

        // If there are both a cache size and a file size, the minimum value is used.
        let policy = MediaRetentionPolicy::empty()
            .with_max_cache_size(Some(200))
            .with_max_file_size(Some(1000));

        // Caching big doesn't work.
        self.add_media_content_inner(
            &request_big,
            content_big.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        self.add_media_content_inner(
            &request_avg,
            content_avg.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();

        let stored = self.get_media_content_inner(&request_big, time).await.unwrap();
        assert!(stored.is_none());
        let stored = self.get_media_content_inner(&request_avg, time).await.unwrap();
        assert!(stored.is_some());

        // Change to a policy that doesn't accept the average media.
        let policy = MediaRetentionPolicy::empty()
            .with_max_cache_size(Some(100))
            .with_max_file_size(Some(1000));

        // The cleanup removes the average media.
        self.clean_up_media_cache_inner(policy, time).await.unwrap();

        let stored = self.get_media_content_inner(&request_avg, time).await.unwrap();
        assert!(stored.is_none());
        let stored = self.get_media_content_inner(&request_small, time).await.unwrap();
        assert!(stored.is_some());

        // Caching big and average media doesn't work.
        self.add_media_content_inner(
            &request_big,
            content_big,
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        self.add_media_content_inner(
            &request_avg,
            content_avg,
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();

        let stored = self.get_media_content_inner(&request_big, time).await.unwrap();
        assert!(stored.is_none());
        let stored = self.get_media_content_inner(&request_avg, time).await.unwrap();
        assert!(stored.is_none());
    }

    async fn test_media_max_cache_size(&self) {
        // 256 bytes content.
        let content_big = vec![0; 256];
        let uri_big = owned_mxc_uri!("mxc://localhost/big-media");
        let request_big = MediaRequestParameters {
            source: MediaSource::Plain(uri_big),
            format: MediaFormat::File,
        };

        // 128 bytes content.
        let content_avg = vec![0; 128];
        let uri_avg = mxc_uri!("mxc://localhost/average-media");
        let request_avg = MediaRequestParameters {
            source: MediaSource::Plain(uri_avg.to_owned()),
            format: MediaFormat::File,
        };

        // 64 bytes content.
        let content_small = vec![0; 64];
        let uri_small_1 = owned_mxc_uri!("mxc://localhost/small-media-1");
        let request_small_1 = MediaRequestParameters {
            source: MediaSource::Plain(uri_small_1),
            format: MediaFormat::File,
        };
        let uri_small_2 = owned_mxc_uri!("mxc://localhost/small-media-2");
        let request_small_2 = MediaRequestParameters {
            source: MediaSource::Plain(uri_small_2),
            format: MediaFormat::File,
        };
        let uri_small_3 = owned_mxc_uri!("mxc://localhost/small-media-3");
        let request_small_3 = MediaRequestParameters {
            source: MediaSource::Plain(uri_small_3),
            format: MediaFormat::File,
        };
        let uri_small_4 = owned_mxc_uri!("mxc://localhost/small-media-4");
        let request_small_4 = MediaRequestParameters {
            source: MediaSource::Plain(uri_small_4),
            format: MediaFormat::File,
        };
        let uri_small_5 = owned_mxc_uri!("mxc://localhost/small-media-5");
        let request_small_5 = MediaRequestParameters {
            source: MediaSource::Plain(uri_small_5),
            format: MediaFormat::File,
        };

        // A policy that doesn't accept the big media.
        let policy = MediaRetentionPolicy::empty().with_max_cache_size(Some(200));

        // Try to add all the content at different times.
        let mut time = SystemTime::UNIX_EPOCH;
        self.add_media_content_inner(
            &request_big,
            content_big,
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_small_1,
            content_small.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_small_2,
            content_small.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_small_3,
            content_small.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_small_4,
            content_small.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_small_5,
            content_small.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_avg,
            content_avg,
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();

        // The big content was NOT cached but the others were.
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_big, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_1, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_2, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_3, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_4, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_5, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_for_uri_inner(uri_avg, time).await.unwrap();
        assert!(stored.is_some());

        // Cleanup removes the oldest content first.
        time += Duration::from_secs(1);
        self.clean_up_media_cache_inner(policy, time).await.unwrap();

        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_1, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_2, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_3, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_4, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_5, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_avg, time).await.unwrap();
        assert!(stored.is_some());

        // Reinsert the small medias that were removed.
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_small_1,
            content_small.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_small_2,
            content_small.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_small_3,
            content_small.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_small_4,
            content_small,
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();

        // Check that they are cached.
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_1, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_2, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_3, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_4, time).await.unwrap();
        assert!(stored.is_some());

        // Access small_5 too so its last access is updated too.
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_5, time).await.unwrap();
        assert!(stored.is_some());

        // Cleanup still removes the oldest content first, which is not the same as
        // before.
        time += Duration::from_secs(1);
        tracing::info!(?self, "before");
        self.clean_up_media_cache_inner(policy, time).await.unwrap();
        tracing::info!(?self, "after");
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_1, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_2, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_3, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_4, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small_5, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_avg, time).await.unwrap();
        assert!(stored.is_none());
    }

    async fn test_media_expiry(&self) {
        // 64 bytes content.
        let content = vec![0; 64];

        let uri_1 = owned_mxc_uri!("mxc://localhost/media-1");
        let request_1 =
            MediaRequestParameters { source: MediaSource::Plain(uri_1), format: MediaFormat::File };
        let uri_2 = owned_mxc_uri!("mxc://localhost/media-2");
        let request_2 =
            MediaRequestParameters { source: MediaSource::Plain(uri_2), format: MediaFormat::File };
        let uri_3 = owned_mxc_uri!("mxc://localhost/media-3");
        let request_3 =
            MediaRequestParameters { source: MediaSource::Plain(uri_3), format: MediaFormat::File };
        let uri_4 = owned_mxc_uri!("mxc://localhost/media-4");
        let request_4 =
            MediaRequestParameters { source: MediaSource::Plain(uri_4), format: MediaFormat::File };
        let uri_5 = owned_mxc_uri!("mxc://localhost/media-5");
        let request_5 =
            MediaRequestParameters { source: MediaSource::Plain(uri_5), format: MediaFormat::File };

        // A policy with 30 seconds expiry.
        let policy =
            MediaRetentionPolicy::empty().with_last_access_expiry(Some(Duration::from_secs(30)));

        // Add all the content at different times.
        let mut time = SystemTime::UNIX_EPOCH;
        self.add_media_content_inner(
            &request_1,
            content.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_2,
            content.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_3,
            content.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_4,
            content.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_5,
            content,
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();

        // The content was cached.
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_1, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_2, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_3, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_4, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_5, time).await.unwrap();
        assert!(stored.is_some());

        // We are now at UNIX_EPOCH + 10 seconds, the oldest content was accessed 5
        // seconds ago.
        time += Duration::from_secs(1);
        assert_eq!(time, SystemTime::UNIX_EPOCH + Duration::from_secs(10));

        // Cleanup has no effect, nothing has expired.
        self.clean_up_media_cache_inner(policy, time).await.unwrap();

        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_1, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_2, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_3, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_4, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_5, time).await.unwrap();
        assert!(stored.is_some());

        // We are now at UNIX_EPOCH + 16 seconds, the oldest content was accessed 5
        // seconds ago.
        time += Duration::from_secs(1);
        assert_eq!(time, SystemTime::UNIX_EPOCH + Duration::from_secs(16));

        // Jump 26 seconds in the future, so the 2 first media contents are expired.
        time += Duration::from_secs(26);

        // Cleanup removes the two oldest media contents.
        self.clean_up_media_cache_inner(policy, time).await.unwrap();

        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_1, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_2, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_3, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_4, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_5, time).await.unwrap();
        assert!(stored.is_some());
    }

    async fn test_media_ignore_max_size(&self) {
        // 256 bytes content.
        let content_big = vec![0; 256];
        let uri_big = owned_mxc_uri!("mxc://localhost/big-media");
        let request_big = MediaRequestParameters {
            source: MediaSource::Plain(uri_big),
            format: MediaFormat::File,
        };

        // 128 bytes content.
        let content_avg = vec![0; 128];
        let uri_avg = mxc_uri!("mxc://localhost/average-media");
        let request_avg = MediaRequestParameters {
            source: MediaSource::Plain(uri_avg.to_owned()),
            format: MediaFormat::File,
        };

        // 64 bytes content.
        let content_small = vec![0; 64];
        let uri_small = owned_mxc_uri!("mxc://localhost/small-media-1");
        let request_small = MediaRequestParameters {
            source: MediaSource::Plain(uri_small),
            format: MediaFormat::File,
        };

        // A policy that will result in only one media content in the cache, which is
        // the average or small content, depending on the last access time.
        let policy = MediaRetentionPolicy::empty().with_max_cache_size(Some(150));

        // Try to add all the big content without ignoring the policy, it should fail.
        let mut time = SystemTime::UNIX_EPOCH;
        self.add_media_content_inner(
            &request_big,
            content_big.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();

        let stored = self.get_media_content_inner(&request_big, time).await.unwrap();
        assert!(stored.is_none());

        // Try to add it again but ignore the policy this time, it should succeed.
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_big,
            content_big,
            time,
            policy,
            IgnoreMediaRetentionPolicy::Yes,
        )
        .await
        .unwrap();

        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_big, time).await.unwrap();
        assert!(stored.is_some());

        // Add the other contents.
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_small,
            content_small.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_avg,
            content_avg,
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();

        // The other contents were added.
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_for_uri_inner(uri_avg, time).await.unwrap();
        assert!(stored.is_some());

        // Ignore the average content for now so the max cache size is not reached.
        self.set_ignore_media_retention_policy_inner(&request_avg, IgnoreMediaRetentionPolicy::Yes)
            .await
            .unwrap();

        // Because the big and average contents are ignored, cleanup has no effect.
        time += Duration::from_secs(1);
        self.clean_up_media_cache_inner(policy, time).await.unwrap();

        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_for_uri_inner(uri_avg, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_big, time).await.unwrap();
        assert!(stored.is_some());

        // Stop ignoring the big media, it should then be cleaned up.
        self.set_ignore_media_retention_policy_inner(&request_big, IgnoreMediaRetentionPolicy::No)
            .await
            .unwrap();

        time += Duration::from_secs(1);
        self.clean_up_media_cache_inner(policy, time).await.unwrap();

        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_for_uri_inner(uri_avg, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_big, time).await.unwrap();
        assert!(stored.is_none());

        // Stop ignoring the average media. Since the cache size is bigger than
        // the max, the content that was not the last accessed should be cleaned up.
        self.set_ignore_media_retention_policy_inner(&request_avg, IgnoreMediaRetentionPolicy::No)
            .await
            .unwrap();

        time += Duration::from_secs(1);
        self.clean_up_media_cache_inner(policy, time).await.unwrap();

        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_small, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_for_uri_inner(uri_avg, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_big, time).await.unwrap();
        assert!(stored.is_none());
    }

    async fn test_media_ignore_expiry(&self) {
        // 64 bytes content.
        let content = vec![0; 64];

        let uri_1 = owned_mxc_uri!("mxc://localhost/media-1");
        let request_1 =
            MediaRequestParameters { source: MediaSource::Plain(uri_1), format: MediaFormat::File };
        let uri_2 = owned_mxc_uri!("mxc://localhost/media-2");
        let request_2 =
            MediaRequestParameters { source: MediaSource::Plain(uri_2), format: MediaFormat::File };
        let uri_3 = owned_mxc_uri!("mxc://localhost/media-3");
        let request_3 =
            MediaRequestParameters { source: MediaSource::Plain(uri_3), format: MediaFormat::File };
        let uri_4 = owned_mxc_uri!("mxc://localhost/media-4");
        let request_4 =
            MediaRequestParameters { source: MediaSource::Plain(uri_4), format: MediaFormat::File };
        let uri_5 = owned_mxc_uri!("mxc://localhost/media-5");
        let request_5 =
            MediaRequestParameters { source: MediaSource::Plain(uri_5), format: MediaFormat::File };

        // A policy with 30 seconds expiry.
        let policy =
            MediaRetentionPolicy::empty().with_last_access_expiry(Some(Duration::from_secs(30)));

        // Add all the content at different times.
        let mut time = SystemTime::UNIX_EPOCH;
        self.add_media_content_inner(
            &request_1,
            content.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::Yes,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_2,
            content.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::Yes,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_3,
            content.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_4,
            content.clone(),
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();
        time += Duration::from_secs(1);
        self.add_media_content_inner(
            &request_5,
            content,
            time,
            policy,
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();

        // The content was cached.
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_1, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_2, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_3, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_4, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_5, time).await.unwrap();
        assert!(stored.is_some());

        // We advance of 120 seconds, all media should be expired.
        time += Duration::from_secs(120);

        // Cleanup removes all the media contents that are not ignored.
        self.clean_up_media_cache_inner(policy, time).await.unwrap();

        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_1, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_2, time).await.unwrap();
        assert!(stored.is_some());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_3, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_4, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_5, time).await.unwrap();
        assert!(stored.is_none());

        // Do no ignore the content anymore.
        self.set_ignore_media_retention_policy_inner(&request_1, IgnoreMediaRetentionPolicy::No)
            .await
            .unwrap();
        self.set_ignore_media_retention_policy_inner(&request_2, IgnoreMediaRetentionPolicy::No)
            .await
            .unwrap();

        // We advance of 120 seconds, all media should be expired again.
        time += Duration::from_secs(120);

        // Cleanup removes the remaining media contents.
        self.clean_up_media_cache_inner(policy, time).await.unwrap();

        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_1, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_2, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_3, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_4, time).await.unwrap();
        assert!(stored.is_none());
        time += Duration::from_secs(1);
        let stored = self.get_media_content_inner(&request_5, time).await.unwrap();
        assert!(stored.is_none());
    }

    async fn test_store_last_media_cleanup_time(&self) {
        let initial = self.last_media_cleanup_time_inner().await.unwrap();
        let new_time = initial.unwrap_or_else(SystemTime::now) + Duration::from_secs(60);

        // With an empty policy.
        let policy = MediaRetentionPolicy::empty();
        self.clean_up_media_cache_inner(policy, new_time).await.unwrap();

        let stored = self.last_media_cleanup_time_inner().await.unwrap();
        assert_eq!(stored, initial);

        // With the default policy.
        let policy = MediaRetentionPolicy::default();
        self.clean_up_media_cache_inner(policy, new_time).await.unwrap();

        let stored = self.last_media_cleanup_time_inner().await.unwrap();
        assert_eq!(stored, Some(new_time));
    }
}

/// Macro building to allow your [`EventCacheStoreMedia`] implementation to run
/// the entire tests suite locally.
///
/// Can be run with the `with_media_size_tests` argument to include more tests
/// about the media cache retention policy based on content size. It is not
/// recommended to run those in encrypted stores because the size of the
/// encrypted content may vary compared to what the tests expect.
///
/// You need to provide an `async fn get_event_cache_store() ->
/// event_cache::store::Result<Store>` that provides a fresh event cache store
/// that implements `EventCacheStoreMedia` on the same level you invoke the
/// macro.
///
/// ## Usage Example:
/// ```no_run
/// # use matrix_sdk_base::event_cache::store::{
/// #    EventCacheStore,
/// #    MemoryStore as MyStore,
/// #    Result as EventCacheStoreResult,
/// # };
///
/// #[cfg(test)]
/// mod tests {
///     use super::{EventCacheStoreResult, MyStore};
///
///     async fn get_event_cache_store() -> EventCacheStoreResult<MyStore> {
///         Ok(MyStore::new())
///     }
///
///     event_cache_store_media_integration_tests!();
/// }
/// ```
#[allow(unused_macros, unused_extern_crates)]
#[macro_export]
macro_rules! event_cache_store_media_integration_tests {
    (with_media_size_tests) => {
        mod event_cache_store_media_integration_tests {
            $crate::event_cache_store_media_integration_tests!(@inner);

            #[async_test]
            async fn test_media_max_file_size() {
                let event_cache_store_media = get_event_cache_store().await.unwrap();
                event_cache_store_media.test_media_max_file_size().await;
            }

            #[async_test]
            async fn test_media_max_cache_size() {
                let event_cache_store_media = get_event_cache_store().await.unwrap();
                event_cache_store_media.test_media_max_cache_size().await;
            }

            #[async_test]
            async fn test_media_ignore_max_size() {
                let event_cache_store_media = get_event_cache_store().await.unwrap();
                event_cache_store_media.test_media_ignore_max_size().await;
            }
        }
    };

    () => {
        mod event_cache_store_media_integration_tests {
            $crate::event_cache_store_media_integration_tests!(@inner);
        }
    };

    (@inner) => {
        use matrix_sdk_test::async_test;
        use $crate::event_cache::store::media::EventCacheStoreMediaIntegrationTests;

        use super::get_event_cache_store;

        #[async_test]
        async fn test_store_media_retention_policy() {
            let event_cache_store_media = get_event_cache_store().await.unwrap();
            event_cache_store_media.test_store_media_retention_policy().await;
        }

        #[async_test]
        async fn test_media_expiry() {
            let event_cache_store_media = get_event_cache_store().await.unwrap();
            event_cache_store_media.test_media_expiry().await;
        }

        #[async_test]
        async fn test_media_ignore_expiry() {
            let event_cache_store_media = get_event_cache_store().await.unwrap();
            event_cache_store_media.test_media_ignore_expiry().await;
        }

        #[async_test]
        async fn test_store_last_media_cleanup_time() {
            let event_cache_store_media = get_event_cache_store().await.unwrap();
            event_cache_store_media.test_store_last_media_cleanup_time().await;
        }
    };
}
