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

//! Configuration to decide whether or not to keep media in the cache, allowing
//! to do periodic cleanups to avoid to have the size of the media cache grow
//! indefinitely.
//!
//! To proceed to a cleanup, first set the [`MediaRetentionPolicy`] to use with
//! [`MediaStore::set_media_retention_policy()`]. Then call
//! [`MediaStore::clean()`].
//!
//! In the future, other settings will allow to run automatic periodic cleanup
//! jobs.
//!
//! [`MediaStore::set_media_retention_policy()`]: crate::media::store::MediaStore::set_media_retention_policy
//! [`MediaStore::clean()`]: crate::media::store::MediaStore::clean

use ruma::time::{Duration, SystemTime};
use serde::{Deserialize, Serialize};

#[cfg(doc)]
use crate::media::store::MediaStore;

/// The retention policy for media content used by the [`MediaStore`].
///
/// [`EventCacheStore`]: crate::event_cache::store::EventCacheStore
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[non_exhaustive]
pub struct MediaRetentionPolicy {
    /// The maximum authorized size of the overall media cache, in bytes.
    ///
    /// The cache size is defined as the sum of the sizes of all the (possibly
    /// encrypted) media contents in the cache, excluding any metadata
    /// associated with them.
    ///
    /// If this is set and the cache size is bigger than this value, the oldest
    /// media contents in the cache will be removed during a cleanup until the
    /// cache size is below this threshold.
    ///
    /// Note that it is possible for the cache size to temporarily exceed this
    /// value between two cleanups.
    ///
    /// Defaults to 400 MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cache_size: Option<u64>,

    /// The maximum authorized size of a single media content, in bytes.
    ///
    /// The size of a media content is the size taken by the content in the
    /// database, after it was possibly encrypted, so it might differ from the
    /// initial size of the content.
    ///
    /// The maximum authorized size of a single media content is actually the
    /// lowest value between `max_cache_size` and `max_file_size`.
    ///
    /// If it is set, media content bigger than the maximum size will not be
    /// cached. If the maximum size changed after media content that exceeds the
    /// new value was cached, the corresponding content will be removed
    /// during a cleanup.
    ///
    /// Defaults to 20 MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_size: Option<u64>,

    /// The duration after which unaccessed media content is considered
    /// expired.
    ///
    /// If this is set, media content whose last access is older than this
    /// duration will be removed from the media cache during a cleanup.
    ///
    /// Defaults to 60 days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_access_expiry: Option<Duration>,

    /// The duration between two automatic media cache cleanups.
    ///
    /// If this is set, a cleanup will be triggered after the given duration
    /// is elapsed, at the next call to the media cache API. If this is set to
    /// zero, each call to the media cache API will trigger a cleanup. If this
    /// is `None`, cleanups will only occur if they are triggered manually.
    ///
    /// Defaults to running cleanups daily.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_frequency: Option<Duration>,
}

impl MediaRetentionPolicy {
    /// Create a [`MediaRetentionPolicy`] with the default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an empty [`MediaRetentionPolicy`].
    ///
    /// This means that all media will be cached and cleanups have no effect.
    pub fn empty() -> Self {
        Self {
            max_cache_size: None,
            max_file_size: None,
            last_access_expiry: None,
            cleanup_frequency: None,
        }
    }

    /// Set the maximum authorized size of the overall media cache, in bytes.
    pub fn with_max_cache_size(mut self, size: Option<u64>) -> Self {
        self.max_cache_size = size;
        self
    }

    /// Set the maximum authorized size of a single media content, in bytes.
    pub fn with_max_file_size(mut self, size: Option<u64>) -> Self {
        self.max_file_size = size;
        self
    }

    /// Set the duration before which unaccessed media content is considered
    /// expired.
    pub fn with_last_access_expiry(mut self, duration: Option<Duration>) -> Self {
        self.last_access_expiry = duration;
        self
    }

    /// Set the duration between two automatic media cache cleanups.
    pub fn with_cleanup_frequency(mut self, duration: Option<Duration>) -> Self {
        self.cleanup_frequency = duration;
        self
    }

    /// Whether this policy has limitations.
    ///
    /// If this policy has no limitations, a cleanup job would have no effect.
    ///
    /// Returns `true` if at least one limitation is set.
    pub fn has_limitations(&self) -> bool {
        self.max_cache_size.is_some()
            || self.max_file_size.is_some()
            || self.last_access_expiry.is_some()
    }

    /// Whether the given size exceeds the maximum authorized size of the media
    /// cache.
    ///
    /// # Arguments
    ///
    /// * `size` - The overall size of the media cache to check, in bytes.
    pub fn exceeds_max_cache_size(&self, size: u64) -> bool {
        self.max_cache_size.is_some_and(|max_size| size > max_size)
    }

    /// The computed maximum authorized size of a single media content, in
    /// bytes.
    ///
    /// This is the lowest value between `max_cache_size` and `max_file_size`.
    pub fn computed_max_file_size(&self) -> Option<u64> {
        match (self.max_cache_size, self.max_file_size) {
            (None, None) => None,
            (None, Some(size)) => Some(size),
            (Some(size), None) => Some(size),
            (Some(max_cache_size), Some(max_file_size)) => Some(max_cache_size.min(max_file_size)),
        }
    }

    /// Whether the given size, in bytes, exceeds the computed maximum
    /// authorized size of a single media content.
    ///
    /// # Arguments
    ///
    /// * `size` - The size of the media content to check, in bytes.
    pub fn exceeds_max_file_size(&self, size: u64) -> bool {
        self.computed_max_file_size().is_some_and(|max_size| size > max_size)
    }

    /// Whether a content whose last access was at the given time has expired.
    ///
    /// # Arguments
    ///
    /// * `current_time` - The current time.
    ///
    /// * `last_access_time` - The time when the media content to check was last
    ///   accessed.
    pub fn has_content_expired(
        &self,
        current_time: SystemTime,
        last_access_time: SystemTime,
    ) -> bool {
        self.last_access_expiry.is_some_and(|max_duration| {
            current_time
                .duration_since(last_access_time)
                // If this returns an error, the last access time is newer than the current time.
                // This shouldn't happen but in this case the content cannot be expired.
                .is_ok_and(|elapsed| elapsed >= max_duration)
        })
    }

    /// Whether an automatic media cache cleanup should be triggered given the
    /// time of the last cleanup.
    ///
    /// # Arguments
    ///
    /// * `current_time` - The current time.
    ///
    /// * `last_cleanup_time` - The time of the last media cache cleanup.
    pub fn should_clean_up(&self, current_time: SystemTime, last_cleanup_time: SystemTime) -> bool {
        self.cleanup_frequency.is_some_and(|max_duration| {
            current_time
                .duration_since(last_cleanup_time)
                // If this returns an error, the last cleanup time is newer than the current time.
                // This shouldn't happen but in this case no cleanup job is needed.
                .is_ok_and(|elapsed| elapsed >= max_duration)
        })
    }
}

impl Default for MediaRetentionPolicy {
    fn default() -> Self {
        Self {
            // 400 MiB.
            max_cache_size: Some(400 * 1024 * 1024),
            // 20 MiB.
            max_file_size: Some(20 * 1024 * 1024),
            // 60 days.
            last_access_expiry: Some(Duration::from_secs(60 * 24 * 60 * 60)),
            // 1 day.
            cleanup_frequency: Some(Duration::from_secs(24 * 60 * 60)),
        }
    }
}

#[cfg(test)]
mod tests {
    use ruma::time::{Duration, SystemTime};

    use super::MediaRetentionPolicy;

    #[test]
    fn test_media_retention_policy_has_limitations() {
        let mut policy = MediaRetentionPolicy::empty();
        assert!(!policy.has_limitations());

        policy = policy.with_last_access_expiry(Some(Duration::from_secs(60)));
        assert!(policy.has_limitations());

        policy = policy.with_last_access_expiry(None);
        assert!(!policy.has_limitations());

        policy = policy.with_max_cache_size(Some(1_024));
        assert!(policy.has_limitations());

        policy = policy.with_max_cache_size(None);
        assert!(!policy.has_limitations());

        policy = policy.with_max_file_size(Some(1_024));
        assert!(policy.has_limitations());

        policy = policy.with_max_file_size(None);
        assert!(!policy.has_limitations());

        // With default values.
        assert!(MediaRetentionPolicy::new().has_limitations());
    }

    #[test]
    fn test_media_retention_policy_max_cache_size() {
        let file_size = 2_048;

        let mut policy = MediaRetentionPolicy::empty();
        assert!(!policy.exceeds_max_cache_size(file_size));
        assert_eq!(policy.computed_max_file_size(), None);
        assert!(!policy.exceeds_max_file_size(file_size));

        policy = policy.with_max_cache_size(Some(4_096));
        assert!(!policy.exceeds_max_cache_size(file_size));
        assert_eq!(policy.computed_max_file_size(), Some(4_096));
        assert!(!policy.exceeds_max_file_size(file_size));

        policy = policy.with_max_cache_size(Some(2_048));
        assert!(!policy.exceeds_max_cache_size(file_size));
        assert_eq!(policy.computed_max_file_size(), Some(2_048));
        assert!(!policy.exceeds_max_file_size(file_size));

        policy = policy.with_max_cache_size(Some(1_024));
        assert!(policy.exceeds_max_cache_size(file_size));
        assert_eq!(policy.computed_max_file_size(), Some(1_024));
        assert!(policy.exceeds_max_file_size(file_size));
    }

    #[test]
    fn test_media_retention_policy_max_file_size() {
        let file_size = 2_048;

        let mut policy = MediaRetentionPolicy::empty();
        assert_eq!(policy.computed_max_file_size(), None);
        assert!(!policy.exceeds_max_file_size(file_size));

        // With max_file_size only.
        policy = policy.with_max_file_size(Some(4_096));
        assert_eq!(policy.computed_max_file_size(), Some(4_096));
        assert!(!policy.exceeds_max_file_size(file_size));

        policy = policy.with_max_file_size(Some(2_048));
        assert_eq!(policy.computed_max_file_size(), Some(2_048));
        assert!(!policy.exceeds_max_file_size(file_size));

        policy = policy.with_max_file_size(Some(1_024));
        assert_eq!(policy.computed_max_file_size(), Some(1_024));
        assert!(policy.exceeds_max_file_size(file_size));

        // With max_cache_size as well.
        policy = policy.with_max_cache_size(Some(2_048));
        assert_eq!(policy.computed_max_file_size(), Some(1_024));
        assert!(policy.exceeds_max_file_size(file_size));

        policy = policy.with_max_file_size(Some(2_048));
        assert_eq!(policy.computed_max_file_size(), Some(2_048));
        assert!(!policy.exceeds_max_file_size(file_size));

        policy = policy.with_max_file_size(Some(4_096));
        assert_eq!(policy.computed_max_file_size(), Some(2_048));
        assert!(!policy.exceeds_max_file_size(file_size));

        policy = policy.with_max_cache_size(Some(1_024));
        assert_eq!(policy.computed_max_file_size(), Some(1_024));
        assert!(policy.exceeds_max_file_size(file_size));
    }

    #[test]
    fn test_media_retention_policy_has_content_expired() {
        let epoch = SystemTime::UNIX_EPOCH;
        let last_access_time = epoch + Duration::from_secs(30);
        let epoch_plus_60 = epoch + Duration::from_secs(60);
        let epoch_plus_120 = epoch + Duration::from_secs(120);

        let mut policy = MediaRetentionPolicy::empty();
        assert!(!policy.has_content_expired(epoch, last_access_time));
        assert!(!policy.has_content_expired(last_access_time, last_access_time));
        assert!(!policy.has_content_expired(epoch_plus_60, last_access_time));
        assert!(!policy.has_content_expired(epoch_plus_120, last_access_time));

        policy = policy.with_last_access_expiry(Some(Duration::from_secs(120)));
        assert!(!policy.has_content_expired(epoch, last_access_time));
        assert!(!policy.has_content_expired(last_access_time, last_access_time));
        assert!(!policy.has_content_expired(epoch_plus_60, last_access_time));
        assert!(!policy.has_content_expired(epoch_plus_120, last_access_time));

        policy = policy.with_last_access_expiry(Some(Duration::from_secs(60)));
        assert!(!policy.has_content_expired(epoch, last_access_time));
        assert!(!policy.has_content_expired(last_access_time, last_access_time));
        assert!(!policy.has_content_expired(epoch_plus_60, last_access_time));
        assert!(policy.has_content_expired(epoch_plus_120, last_access_time));

        policy = policy.with_last_access_expiry(Some(Duration::from_secs(30)));
        assert!(!policy.has_content_expired(epoch, last_access_time));
        assert!(!policy.has_content_expired(last_access_time, last_access_time));
        assert!(policy.has_content_expired(epoch_plus_60, last_access_time));
        assert!(policy.has_content_expired(epoch_plus_120, last_access_time));

        policy = policy.with_last_access_expiry(Some(Duration::from_secs(0)));
        assert!(!policy.has_content_expired(epoch, last_access_time));
        assert!(policy.has_content_expired(last_access_time, last_access_time));
        assert!(policy.has_content_expired(epoch_plus_60, last_access_time));
        assert!(policy.has_content_expired(epoch_plus_120, last_access_time));
    }

    #[test]
    fn test_media_retention_policy_cleanup_frequency() {
        let epoch = SystemTime::UNIX_EPOCH;
        let epoch_plus_60 = epoch + Duration::from_secs(60);
        let epoch_plus_120 = epoch + Duration::from_secs(120);

        let mut policy = MediaRetentionPolicy::empty();
        assert!(!policy.should_clean_up(epoch_plus_60, epoch));
        assert!(!policy.should_clean_up(epoch_plus_60, epoch_plus_60));
        assert!(!policy.should_clean_up(epoch_plus_60, epoch_plus_120));

        policy = policy.with_cleanup_frequency(Some(Duration::from_secs(0)));
        assert!(policy.should_clean_up(epoch_plus_60, epoch));
        assert!(policy.should_clean_up(epoch_plus_60, epoch_plus_60));
        assert!(!policy.should_clean_up(epoch_plus_60, epoch_plus_120));

        policy = policy.with_cleanup_frequency(Some(Duration::from_secs(30)));
        assert!(policy.should_clean_up(epoch_plus_60, epoch));
        assert!(!policy.should_clean_up(epoch_plus_60, epoch_plus_60));
        assert!(!policy.should_clean_up(epoch_plus_60, epoch_plus_120));

        policy = policy.with_cleanup_frequency(Some(Duration::from_secs(60)));
        assert!(policy.should_clean_up(epoch_plus_60, epoch));
        assert!(!policy.should_clean_up(epoch_plus_60, epoch_plus_60));
        assert!(!policy.should_clean_up(epoch_plus_60, epoch_plus_120));

        policy = policy.with_cleanup_frequency(Some(Duration::from_secs(90)));
        assert!(!policy.should_clean_up(epoch_plus_60, epoch));
        assert!(!policy.should_clean_up(epoch_plus_60, epoch_plus_60));
        assert!(!policy.should_clean_up(epoch_plus_60, epoch_plus_120));
    }
}
