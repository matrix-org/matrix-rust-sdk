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
