// Copyright 2025 The Matrix.org Foundation C.I.C.
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
// limitations under the License

use std::{
    ops::{Add, Deref, Sub},
    time::Duration,
};

use matrix_sdk_base::{
    cross_process_lock::CrossProcessLockGeneration,
    media::{MediaRequestParameters, store::IgnoreMediaRetentionPolicy},
};
use ruma::time::{SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Representation of a time-based lock on the entire
/// [`IndexeddbMediaStore`](crate::media_store::IndexeddbMediaStore)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub key: String,
    pub holder: String,
    pub expiration: Duration,
    pub generation: CrossProcessLockGeneration,
}

/// A representation of media which ignores storage schemas. This is type is not
/// stored in IndexedDB, and is mostly useful for passing media around which may
/// eventually be transformed into types which are storable in IndexedDB.
#[derive(Debug)]
pub struct Media {
    /// The parameters specifying the type and source of the media contained in
    /// [`Media::content`]
    pub request_parameters: MediaRequestParameters,
    /// The last time the media was accessed in IndexedDB
    pub last_access: UnixTime,
    /// Whether to ignore the [`MediaRetentionPolicy`][1] stored in IndexedDB
    ///
    /// [1]: matrix_sdk_base::media::store::MediaRetentionPolicy
    pub ignore_policy: IgnoreMediaRetentionPolicy,
    /// The content of the media
    pub content: Vec<u8>,
}

/// A representation of media metadata which can be stored in IndexedDB.
#[derive(Debug, Serialize, Deserialize)]
pub struct MediaMetadata {
    /// The parameters specifying the type and source of the associated
    /// [`MediaContent`]
    pub request_parameters: MediaRequestParameters,
    /// The last time the associated [`MediaContent`] was accessed in IndexedDB
    pub last_access: UnixTime,
    /// Whether to ignore the [`MediaRetentionPolicy`][1] stored in IndexedDB
    ///
    /// [1]: matrix_sdk_base::media::store::MediaRetentionPolicy
    #[serde(with = "crate::media_store::serializer::foreign::ignore_media_retention_policy")]
    pub ignore_policy: IgnoreMediaRetentionPolicy,
    /// The identifier of the associated [`MediaContent`]
    pub content_id: Uuid,
    /// The size in bytes of the associated [`MediaContent`]
    pub content_size: usize,
}

/// A representation of media content which can be stored in IndexedDB.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaContent {
    /// The identifier associated with the given [`MediaContent::data`].
    pub content_id: Uuid,
    /// The bytes to be stored in IndexedDB
    pub data: Vec<u8>,
}

/// A representation of time relative to the [`UNIX_EPOCH`].
///
/// Typically a type of this nature is represented as a [`Duration`],
/// but the conversion from a [`SystemTime`] to a [`Duration`] is
/// fallible (see [`SystemTime::duration_since`]). The benefit of this
/// type is that it can provide an infallible conversion, excepting
/// overflows.
#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub enum UnixTime {
    /// A representation of a point in time before the [`UNIX_EPOCH`], which is
    /// quantified by the nested [`Duration`]
    BeforeEpoch(Duration),
    /// A representation of a point in time after the [`UNIX_EPOCH`], which is
    /// quantified by the nested [`Duration`]
    AfterEpoch(Duration),
}

impl From<SystemTime> for UnixTime {
    fn from(value: SystemTime) -> Self {
        match value.duration_since(UNIX_EPOCH) {
            Ok(duration) => Self::AfterEpoch(duration),
            Err(e) => Self::BeforeEpoch(e.duration()),
        }
    }
}

impl From<UnixTime> for SystemTime {
    fn from(value: UnixTime) -> Self {
        match value {
            UnixTime::BeforeEpoch(duration) => UNIX_EPOCH - duration,
            UnixTime::AfterEpoch(duration) => UNIX_EPOCH + duration,
        }
    }
}

impl Add<Duration> for UnixTime {
    type Output = Self;

    fn add(self, rhs: Duration) -> Self::Output {
        match self {
            Self::BeforeEpoch(duration) => {
                // When a time is before the Unix Epoch, adding a duration
                // means moving towards the epoch, and possibly crossing it.
                if rhs > duration {
                    // If we are adding a duration larger than the internal
                    // duration, then we are crossing the Unix Epoch
                    Self::AfterEpoch(rhs - duration)
                } else {
                    // Otherwise, we are simply moving towards the epoch.
                    Self::BeforeEpoch(duration - rhs)
                }
            }
            Self::AfterEpoch(duration) => {
                // Once we have crossed the Unix Epoch, we can move forward
                // by adding time without concern for the epoch.
                Self::AfterEpoch(duration + rhs)
            }
        }
    }
}

impl Sub<Duration> for UnixTime {
    type Output = Self;

    fn sub(self, rhs: Duration) -> Self::Output {
        match self {
            Self::AfterEpoch(duration) => {
                // When a time is after the Unix Epoch, subtracting a duration
                // means moving towards the epoch, and possibly crossing it.
                if rhs > duration {
                    // If we are subtracting a duration larger than the internal
                    // duration, then we are crossing the Unix Epoch
                    Self::BeforeEpoch(rhs - duration)
                } else {
                    // Otherwise, we are simply moving towards the epoch.
                    Self::AfterEpoch(duration - rhs)
                }
            }
            Self::BeforeEpoch(duration) => {
                // Once we have crossed the Unix Epoch, we can move backward
                // by adding time without concern for the epoch.
                Self::BeforeEpoch(duration + rhs)
            }
        }
    }
}

/// A newtype-style wrapper around [`UnixTime`] which represents a time at which
/// the media store was cleaned.
#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MediaCleanupTime(UnixTime);

impl Deref for MediaCleanupTime {
    type Target = UnixTime;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<UnixTime> for MediaCleanupTime {
    fn as_ref(&self) -> &UnixTime {
        self.deref()
    }
}

impl From<SystemTime> for MediaCleanupTime {
    fn from(value: SystemTime) -> Self {
        Self::from(UnixTime::from(value))
    }
}

impl From<MediaCleanupTime> for SystemTime {
    fn from(value: MediaCleanupTime) -> Self {
        Self::from(UnixTime::from(value))
    }
}

impl From<UnixTime> for MediaCleanupTime {
    fn from(value: UnixTime) -> Self {
        Self(value)
    }
}

impl From<MediaCleanupTime> for UnixTime {
    fn from(value: MediaCleanupTime) -> Self {
        value.0
    }
}
