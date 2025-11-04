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

//! Trait and macro of integration tests for `MediaStoreInner`
//! implementations.

use ruma::{
    events::room::MediaSource,
    media::Method,
    mxc_uri, owned_mxc_uri,
    time::{Duration, SystemTime},
    uint,
};

use super::{MediaRetentionPolicy, MediaStoreInner, media_service::IgnoreMediaRetentionPolicy};
use crate::media::{
    MediaFormat, MediaRequestParameters, MediaThumbnailSettings, store::MediaStore,
};

/// [`MediaStoreInner`] integration tests.
///
/// This trait is not meant to be used directly, but will be used with the
/// `media_store_inner_integration_tests!` macro.
#[allow(async_fn_in_trait)]
pub trait MediaStoreInnerIntegrationTests {
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

impl<Store> MediaStoreInnerIntegrationTests for Store
where
    Store: MediaStoreInner + std::fmt::Debug,
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
        self.clean_inner(policy, time).await.unwrap();

        let stored = self.get_media_content_inner(&request_avg, time).await.unwrap();
        assert!(stored.is_some());
        let stored = self.get_media_content_inner(&request_small, time).await.unwrap();
        assert!(stored.is_some());

        // Change to a policy that doesn't accept the average media.
        let policy = MediaRetentionPolicy::empty().with_max_file_size(Some(100));

        // The cleanup removes the average media.
        self.clean_inner(policy, time).await.unwrap();

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
        self.clean_inner(policy, time).await.unwrap();

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
        self.clean_inner(policy, time).await.unwrap();

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
        self.clean_inner(policy, time).await.unwrap();
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
        self.clean_inner(policy, time).await.unwrap();

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
        self.clean_inner(policy, time).await.unwrap();

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
        self.clean_inner(policy, time).await.unwrap();

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
        self.clean_inner(policy, time).await.unwrap();

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
        self.clean_inner(policy, time).await.unwrap();

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
        self.clean_inner(policy, time).await.unwrap();

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
        self.clean_inner(policy, time).await.unwrap();

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
        self.clean_inner(policy, new_time).await.unwrap();

        let stored = self.last_media_cleanup_time_inner().await.unwrap();
        assert_eq!(stored, initial);

        // With the default policy.
        let policy = MediaRetentionPolicy::default();
        self.clean_inner(policy, new_time).await.unwrap();

        let stored = self.last_media_cleanup_time_inner().await.unwrap();
        assert_eq!(stored, Some(new_time));
    }
}

/// Macro building to allow your [`MediaStoreInner`] implementation to run
/// the entire tests suite locally.
///
/// Can be run with the `with_media_size_tests` argument to include more tests
/// about the media cache retention policy based on content size. It is not
/// recommended to run those in encrypted stores because the size of the
/// encrypted content may vary compared to what the tests expect.
///
/// You need to provide an `async fn get_media_store() ->
/// media::store::Result<Store>` that provides a fresh media store
/// that implements `MediaStoreInner` on the same level you invoke the
/// macro.
///
/// ## Usage Example:
/// ```no_run
/// # use matrix_sdk_base::media::store::{
/// #    MediaStore,
/// #    MemoryMediaStore as MyStore,
/// #    Result as MediaStoreResult,
/// # };
///
/// #[cfg(test)]
/// mod tests {
///     use super::{MediaStoreResult, MyStore};
///
///     async fn get_media_store() -> MediaStoreResult<MyStore> {
///         Ok(MyStore::new())
///     }
///
///     media_store_inner_integration_tests!();
/// }
/// ```
#[allow(unused_macros, unused_extern_crates)]
#[macro_export]
macro_rules! media_store_inner_integration_tests {
    (with_media_size_tests) => {
        mod media_store_inner_integration_tests {
            $crate::media_store_inner_integration_tests!(@inner);

            #[async_test]
            async fn test_media_max_file_size() {
                let media_store_inner = get_media_store().await.unwrap();
                media_store_inner.test_media_max_file_size().await;
            }

            #[async_test]
            async fn test_media_max_cache_size() {
                let media_store_inner = get_media_store().await.unwrap();
                media_store_inner.test_media_max_cache_size().await;
            }

            #[async_test]
            async fn test_media_ignore_max_size() {
                let media_store_inner = get_media_store().await.unwrap();
                media_store_inner.test_media_ignore_max_size().await;
            }
        }
    };

    () => {
        mod media_store_inner_integration_tests {
            $crate::media_store_inner_integration_tests!(@inner);
        }
    };

    (@inner) => {
        use matrix_sdk_test::async_test;
        use $crate::media::store::MediaStoreInnerIntegrationTests;

        use super::get_media_store;

        #[async_test]
        async fn test_store_media_retention_policy() {
            let media_store_inner = get_media_store().await.unwrap();
            media_store_inner.test_store_media_retention_policy().await;
        }

        #[async_test]
        async fn test_media_expiry() {
            let media_store_inner = get_media_store().await.unwrap();
            media_store_inner.test_media_expiry().await;
        }

        #[async_test]
        async fn test_media_ignore_expiry() {
            let media_store_inner = get_media_store().await.unwrap();
            media_store_inner.test_media_ignore_expiry().await;
        }

        #[async_test]
        async fn test_store_last_media_cleanup_time() {
            let media_store_inner = get_media_store().await.unwrap();
            media_store_inner.test_store_last_media_cleanup_time().await;
        }
    };
}

/// [`MediaStore`] integration tests.
///
/// This trait is not meant to be used directly, but will be used with the
/// `media_store_inner_integration_tests!` macro.
#[allow(async_fn_in_trait)]
pub trait MediaStoreIntegrationTests {
    /// Test media content storage.
    async fn test_media_content(&self);

    /// Test replacing a MXID.
    async fn test_replace_media_key(&self);
}

impl<Store> MediaStoreIntegrationTests for Store
where
    Store: MediaStore + std::fmt::Debug,
{
    async fn test_media_content(&self) {
        let uri = mxc_uri!("mxc://localhost/media");
        let request_file = MediaRequestParameters {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::File,
        };
        let request_thumbnail = MediaRequestParameters {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::Thumbnail(MediaThumbnailSettings::with_method(
                Method::Crop,
                uint!(100),
                uint!(100),
            )),
        };

        let other_uri = mxc_uri!("mxc://localhost/media-other");
        let request_other_file = MediaRequestParameters {
            source: MediaSource::Plain(other_uri.to_owned()),
            format: MediaFormat::File,
        };

        let content: Vec<u8> = "hello".into();
        let thumbnail_content: Vec<u8> = "world".into();
        let other_content: Vec<u8> = "foo".into();

        // Media isn't present in the cache.
        assert!(
            self.get_media_content(&request_file).await.unwrap().is_none(),
            "unexpected media found"
        );
        assert!(
            self.get_media_content(&request_thumbnail).await.unwrap().is_none(),
            "media not found"
        );

        // Let's add the media.
        self.add_media_content(&request_file, content.clone(), IgnoreMediaRetentionPolicy::No)
            .await
            .expect("adding media failed");

        // Media is present in the cache.
        assert_eq!(
            self.get_media_content(&request_file).await.unwrap().as_ref(),
            Some(&content),
            "media not found though added"
        );
        assert_eq!(
            self.get_media_content_for_uri(uri).await.unwrap().as_ref(),
            Some(&content),
            "media not found by URI though added"
        );

        // Let's remove the media.
        self.remove_media_content(&request_file).await.expect("removing media failed");

        // Media isn't present in the cache.
        assert!(
            self.get_media_content(&request_file).await.unwrap().is_none(),
            "media still there after removing"
        );
        assert!(
            self.get_media_content_for_uri(uri).await.unwrap().is_none(),
            "media still found by URI after removing"
        );

        // Let's add the media again.
        self.add_media_content(&request_file, content.clone(), IgnoreMediaRetentionPolicy::No)
            .await
            .expect("adding media again failed");

        assert_eq!(
            self.get_media_content(&request_file).await.unwrap().as_ref(),
            Some(&content),
            "media not found after adding again"
        );

        // Let's add the thumbnail media.
        self.add_media_content(
            &request_thumbnail,
            thumbnail_content.clone(),
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .expect("adding thumbnail failed");

        // Media's thumbnail is present.
        assert_eq!(
            self.get_media_content(&request_thumbnail).await.unwrap().as_ref(),
            Some(&thumbnail_content),
            "thumbnail not found"
        );

        // We get a file with the URI, we don't know which one.
        assert!(
            self.get_media_content_for_uri(uri).await.unwrap().is_some(),
            "media not found by URI though two where added"
        );

        // Let's add another media with a different URI.
        self.add_media_content(
            &request_other_file,
            other_content.clone(),
            IgnoreMediaRetentionPolicy::No,
        )
        .await
        .expect("adding other media failed");

        // Other file is present.
        assert_eq!(
            self.get_media_content(&request_other_file).await.unwrap().as_ref(),
            Some(&other_content),
            "other file not found"
        );
        assert_eq!(
            self.get_media_content_for_uri(other_uri).await.unwrap().as_ref(),
            Some(&other_content),
            "other file not found by URI"
        );

        // Let's remove media based on URI.
        self.remove_media_content_for_uri(uri).await.expect("removing all media for uri failed");

        assert!(
            self.get_media_content(&request_file).await.unwrap().is_none(),
            "media wasn't removed"
        );
        assert!(
            self.get_media_content(&request_thumbnail).await.unwrap().is_none(),
            "thumbnail wasn't removed"
        );
        assert!(
            self.get_media_content(&request_other_file).await.unwrap().is_some(),
            "other media was removed"
        );
        assert!(
            self.get_media_content_for_uri(uri).await.unwrap().is_none(),
            "media found by URI wasn't removed"
        );
        assert!(
            self.get_media_content_for_uri(other_uri).await.unwrap().is_some(),
            "other media found by URI was removed"
        );
    }

    async fn test_replace_media_key(&self) {
        let uri = mxc_uri!("mxc://sendqueue.local/tr4n-s4ct-10n1-d");
        let req = MediaRequestParameters {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::File,
        };

        let content = "hello".as_bytes().to_owned();

        // Media isn't present in the cache.
        assert!(self.get_media_content(&req).await.unwrap().is_none(), "unexpected media found");

        // Add the media.
        self.add_media_content(&req, content.clone(), IgnoreMediaRetentionPolicy::No)
            .await
            .expect("adding media failed");

        // Sanity-check: media is found after adding it.
        assert_eq!(self.get_media_content(&req).await.unwrap().unwrap(), b"hello");

        // Replacing a media request works.
        let new_uri = mxc_uri!("mxc://matrix.org/tr4n-s4ct-10n1-d");
        let new_req = MediaRequestParameters {
            source: MediaSource::Plain(new_uri.to_owned()),
            format: MediaFormat::File,
        };
        self.replace_media_key(&req, &new_req)
            .await
            .expect("replacing the media request key failed");

        // Finding with the previous request doesn't work anymore.
        assert!(
            self.get_media_content(&req).await.unwrap().is_none(),
            "unexpected media found with the old key"
        );

        // Finding with the new request does work.
        assert_eq!(self.get_media_content(&new_req).await.unwrap().unwrap(), b"hello");
    }
}

/// Macro building to allow your [`MediaStore`] implementation to run
/// the entire tests suite locally.
///
/// You need to provide an `async fn get_media_store() ->
/// media::store::Result<Store>` that provides a fresh media store
/// that implements `MediaStoreInner` on the same level you invoke the
/// macro.
///
/// ## Usage Example:
/// ```no_run
/// # use matrix_sdk_base::media::store::{
/// #    MediaStore,
/// #    MemoryMediaStore as MyStore,
/// #    Result as MediaStoreResult,
/// # };
///
/// #[cfg(test)]
/// mod tests {
///     use super::{MediaStoreResult, MyStore};
///
///     async fn get_media_store() -> MediaStoreResult<MyStore> {
///         Ok(MyStore::new())
///     }
///
///     media_store_integration_tests!();
/// }
/// ```
#[allow(unused_macros, unused_extern_crates)]
#[macro_export]
macro_rules! media_store_integration_tests {
    () => {
        mod media_store_integration_tests {
            use matrix_sdk_test::async_test;
            use $crate::media::store::integration_tests::MediaStoreIntegrationTests;

            use super::get_media_store;

            #[async_test]
            async fn test_media_content() {
                let media_store = get_media_store().await.unwrap();
                media_store.test_media_content().await;
            }

            #[async_test]
            async fn test_replace_media_key() {
                let media_store = get_media_store().await.unwrap();
                media_store.test_replace_media_key().await;
            }
        }
    };
}

/// Macro generating tests for the media store, related to time (mostly
/// for the cross-process lock).
#[allow(unused_macros)]
#[macro_export]
macro_rules! media_store_integration_tests_time {
    () => {
        mod media_store_integration_tests_time {
            use std::time::Duration;

            #[cfg(all(target_family = "wasm", target_os = "unknown"))]
            use gloo_timers::future::sleep;
            use matrix_sdk_test::async_test;
            #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
            use tokio::time::sleep;
            use $crate::media::store::MediaStore;

            use super::get_media_store;

            #[async_test]
            async fn test_lease_locks() {
                let store = get_media_store().await.unwrap();

                let acquired0 = store.try_take_leased_lock(0, "key", "alice").await.unwrap();
                assert_eq!(acquired0, Some(1)); // first lock generation

                // Should extend the lease automatically (same holder).
                let acquired2 = store.try_take_leased_lock(300, "key", "alice").await.unwrap();
                assert_eq!(acquired2, Some(1)); // same lock generation

                // Should extend the lease automatically (same holder + time is ok).
                let acquired3 = store.try_take_leased_lock(300, "key", "alice").await.unwrap();
                assert_eq!(acquired3, Some(1)); // same lock generation

                // Another attempt at taking the lock should fail, because it's taken.
                let acquired4 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(acquired4.is_none()); // not acquired

                // Even if we insist.
                let acquired5 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(acquired5.is_none()); // not acquired

                // That's a nice test we got here, go take a little nap.
                sleep(Duration::from_millis(50)).await;

                // Still too early.
                let acquired55 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert!(acquired55.is_none()); // not acquired

                // Ok you can take another nap then.
                sleep(Duration::from_millis(250)).await;

                // At some point, we do get the lock.
                let acquired6 = store.try_take_leased_lock(0, "key", "bob").await.unwrap();
                assert_eq!(acquired6, Some(2)); // new lock generation!

                sleep(Duration::from_millis(1)).await;

                // The other gets it almost immediately too.
                let acquired7 = store.try_take_leased_lock(0, "key", "alice").await.unwrap();
                assert_eq!(acquired7, Some(3)); // new lock generation!

                sleep(Duration::from_millis(1)).await;

                // But when we take a longer lease…
                let acquired8 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert_eq!(acquired8, Some(4)); // new lock generation!

                // It blocks the other user.
                let acquired9 = store.try_take_leased_lock(300, "key", "alice").await.unwrap();
                assert!(acquired9.is_none()); // not acquired

                // We can hold onto our lease.
                let acquired10 = store.try_take_leased_lock(300, "key", "bob").await.unwrap();
                assert_eq!(acquired10, Some(4)); // same lock generation
            }
        }
    };
}
