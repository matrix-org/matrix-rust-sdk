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

pub mod ignore_media_retention_policy {
    //! This module contains a foreign implementation of [`serde::Serialize`]
    //! and [`serde::Deserialize`] for [`IgnoreMediaRetentionPolicy`]. These
    //! implementations can be injected with the proper macros, i.e.,
    //! `#[serde(with = "path::to::this::module")]`.
    //!
    //! This is necessary, as [`IgnoreMediaRetentionPolicy`] does not implement
    //! these traits directly.

    use matrix_sdk_base::media::store::IgnoreMediaRetentionPolicy;
    use serde::{Deserializer, Serializer};

    /// Serializes an [`IgnoreMediaRetentionPolicy`] as a `u8`, where
    /// [`IgnoreMediaRetentionPolicy::No`] is `0`
    /// and [`IgnoreMediaRetentionPolicy::Yes`] is `1`.
    ///
    /// Note that this is not serialized as a `bool` because boolean values are
    /// not supported as IndexedDB keys.
    pub fn serialize<S>(ignore_policy: &IgnoreMediaRetentionPolicy, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_u8(match ignore_policy {
            IgnoreMediaRetentionPolicy::Yes => 1,
            IgnoreMediaRetentionPolicy::No => 0,
        })
    }

    /// Deserializes a `u8` into an [`IgnoreMediaRetentionPolicy`] where `0` is
    /// [`IgnoreMediaRetentionPolicy::No`] and anything else is
    /// [`IgnoreMediaRetentionPolicy::Yes`].
    pub fn deserialize<'de, D>(d: D) -> Result<IgnoreMediaRetentionPolicy, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match serde::de::Deserialize::deserialize(d)? {
            0u8 => IgnoreMediaRetentionPolicy::No,
            _ => IgnoreMediaRetentionPolicy::Yes,
        })
    }
}

pub mod unix_time {
    //! This module contains an alternative implementation of
    //! [`serde::Serialize`] and [`serde::Deserialize`] for [`UnixTime`].
    //! These implementations can be injected with the proper macros, i.e.,
    //! `#[serde(with = "path::to::this::module")]`.
    //!
    //! This is necessary, as the derived implementation of [`UnixTime`] does
    //! not produce values which can be used in IndexedDB keys.

    use std::time::Duration;

    use serde::{Deserializer, Serializer};

    use crate::media_store::types::UnixTime;

    /// Serializes a [`UnixTime`] as an `i64` which represents an amount of
    /// seconds relative to the [`UNIX_EPOCH`](ruma::time::UNIX_EPOCH).
    /// [`UnixTime::BeforeEpoch`] is represented as a negative value
    /// and [`UnixTime::AfterEpoch`] is represented as a positive value.
    pub fn serialize<S>(unix_time: &UnixTime, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_i64(match unix_time {
            UnixTime::BeforeEpoch(duration) => {
                -i64::try_from(duration.as_secs()).map_err(serde::ser::Error::custom)?
            }
            UnixTime::AfterEpoch(duration) => {
                i64::try_from(duration.as_secs()).map_err(serde::ser::Error::custom)?
            }
        })
    }

    /// Deserializes an `i64` into a [`UnixTime`]. Negative values represent
    /// the number of seconds before the [`UNIX_EPOCH`][1] and are deserialized
    /// into [`UnixTime::BeforeEpoch`]. Positive values represent
    /// the number of seconds after the [`UNIX_EPOCH`][1] and are deserialized
    /// into [`UnixTime::AfterEpoch`].
    ///
    /// [1]: ruma::time::UNIX_EPOCH
    pub fn deserialize<'de, D>(d: D) -> Result<UnixTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let seconds: i64 = serde::de::Deserialize::deserialize(d)?;
        Ok(match seconds {
            seconds @ ..0 => UnixTime::BeforeEpoch(Duration::from_secs(seconds.unsigned_abs())),
            seconds @ 0.. => UnixTime::AfterEpoch(Duration::from_secs(seconds.unsigned_abs())),
        })
    }
}
