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

use std::{sync::LazyLock, time::Duration};

use uuid::Uuid;

/// The first unicode character, and hence the lower bound for IndexedDB keys
/// (or key components) which are represented as strings.
///
/// This value is useful for constructing a key range over all strings when used
/// in conjunction with [`INDEXED_KEY_UPPER_CHARACTER`].
pub const INDEXED_KEY_LOWER_CHARACTER: char = '\u{0000}';

/// The last unicode character in the [Basic Multilingual Plane][1]. This seems
/// like a reasonable place to set the upper bound for IndexedDB keys (or key
/// components) which are represented as strings, though one could
/// theoretically set it to `\u{10FFFF}`.
///
/// This value is useful for constructing a key range over all strings when used
/// in conjunction with [`INDEXED_KEY_LOWER_CHARACTER`].
///
/// [1]: https://en.wikipedia.org/wiki/Plane_(Unicode)#Basic_Multilingual_Plane
pub const INDEXED_KEY_UPPER_CHARACTER: char = '\u{FFFF}';

/// Identical to [`INDEXED_KEY_LOWER_CHARACTER`] but represented as a [`String`]
pub static INDEXED_KEY_LOWER_STRING: LazyLock<String> =
    LazyLock::new(|| String::from(INDEXED_KEY_LOWER_CHARACTER));

/// Identical to [`INDEXED_KEY_UPPER_CHARACTER`] but represented as a [`String`]
pub static INDEXED_KEY_UPPER_STRING: LazyLock<String> =
    LazyLock::new(|| String::from(INDEXED_KEY_UPPER_CHARACTER));

/// The minimum possible [`u64`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`u64`] values when used in conjunction with
/// [`INDEXED_KEY_UPPER_U64`].
pub const INDEXED_KEY_LOWER_U64: u64 = u64::MIN;

/// The maximum [`u64`] which is expressible in IndexedDB - i.e.,
/// [`js_sys::Number::MAX_SAFE_INTEGER`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`u64`] values when used in conjunction with
/// [`INDEXED_KEY_LOWER_U64`].
pub const INDEXED_KEY_UPPER_U64: u64 = js_sys::Number::MAX_SAFE_INTEGER as u64;

/// The minimum possible [`Uuid`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`Uuid`] values when used in conjunction with
/// [`INDEXED_KEY_UPPER_UUID`].
pub const INDEXED_KEY_LOWER_UUID: Uuid = Uuid::from_u128(u128::MIN);

/// The maximum possible [`Uuid`]. Note that this is not limited by
/// [`js_sys::Number::MAX_SAFE_INTEGER`] as the [`Uuid`]s are serialized
/// either as bytes or a string.
///
/// This value is useful for constructing a key range over all keys which
/// contain [`Uuid`] values when used in conjunction with
/// [`INDEXED_KEY_LOWER_UUID`].
pub const INDEXED_KEY_UPPER_UUID: Uuid = Uuid::from_u128(u128::MAX);

/// The minimum possible [`Duration`].
///
/// This value is useful for constructing a key range over all keys which
/// contain time-related values when used in conjunction with
/// [`INDEXED_KEY_UPPER_DURATION`].
pub const INDEXED_KEY_LOWER_DURATION: Duration = Duration::ZERO;

/// A [`Duration`] constructed with [`INDEXED_KEY_UPPER_U64`]
/// seconds.
///
/// This value is useful for constructing a key range over all keys which
/// contain time-related values in seconds when used in conjunction with
/// [`INDEXED_KEY_LOWER_DURATION`].
pub const INDEXED_KEY_UPPER_DURATION_SECONDS: Duration = Duration::from_secs(INDEXED_KEY_UPPER_U64);
