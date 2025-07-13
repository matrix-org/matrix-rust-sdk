// Copyright 2023 The Matrix.org Foundation C.I.C.
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

//! Booleans don't work as keys in indexeddb (see [ECMA spec]), so instead we
//! serialize them as `0` or `1`.
//!
//! This module implements a custom serializer which can be used on `bool`
//! struct fields with:
//!
//! ```ignore
//! #[serde(with = "serialize_bool_for_indexeddb")]
//! ```
//!
//! [ECMA spec]: https://w3c.github.io/IndexedDB/#key
use serde::{Deserializer, Serializer};

pub fn serialize<S>(v: &bool, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_u8(if *v { 1 } else { 0 })
}

pub fn deserialize<'de, D>(d: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let v: u8 = serde::de::Deserialize::deserialize(d)?;
    Ok(v != 0)
}
