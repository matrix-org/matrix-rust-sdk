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

use crate::{
    media_store::{
        serializer::indexed_types::{IndexedMediaContentId, IndexedMediaContentSize},
        types::UnixTime,
    },
    serializer::indexed_type::constants::{
        INDEXED_KEY_LOWER_UUID, INDEXED_KEY_UPPER_DURATION_SECONDS, INDEXED_KEY_UPPER_UUID,
    },
};

/// An [`IndexedMediaContentSize`] set to it's minimal value - i.e., `0`.
///
/// This value is useful for constructing a key range over all keys which
/// contain [`IndexedMediaContentSize`] values when used in conjunction with
/// [`INDEXED_KEY_UPPER_MEDIA_CONTENT_SIZE`].
pub const INDEXED_KEY_LOWER_MEDIA_CONTENT_SIZE: IndexedMediaContentSize = 0;

/// An [`IndexedMediaContentSize`] set to [`js_sys::Number::MAX_SAFE_INTEGER`].
/// Note that this restricts the size of [`IndexedMedia::content`], which
/// ultimately restricts the size of [`Media::content`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`IndexedMediaContentSize`] values when used in conjunction with
/// [`INDEXED_KEY_LOWER_MEDIA_CONTENT_SIZE`].
pub const INDEXED_KEY_UPPER_MEDIA_CONTENT_SIZE: IndexedMediaContentSize =
    js_sys::Number::MAX_SAFE_INTEGER as usize;

/// The earliest [`UnixTime`] which can be represented in IndexedDB.
///
/// This value is useful for constructing a key range over all keys which
/// contain time-related values when used in conjunction with
/// [`INDEXED_KEY_UPPER_UNIX_TIME`].
pub const INDEXED_KEY_LOWER_UNIX_TIME: UnixTime =
    UnixTime::BeforeEpoch(INDEXED_KEY_UPPER_DURATION_SECONDS);

/// The latest [`UnixTime`] which can be represented in IndexedDB.
///
/// This value is useful for constructing a key range over all keys which
/// contain time-related values when used in conjunction with
/// [`INDEXED_KEY_LOWER_UNIX_TIME`].
pub const INDEXED_KEY_UPPER_UNIX_TIME: UnixTime =
    UnixTime::AfterEpoch(INDEXED_KEY_UPPER_DURATION_SECONDS);

/// The minimum value for an [`IndexedMediaContentId`] - i.e.,
/// [`INDEXED_KEY_LOWER_UUID`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`IndexedMediaContentId`] values when used in conjunction with
/// [`INDEXED_KEY_UPPER_MEDIA_CONTENT_ID`].
pub const INDEXED_KEY_LOWER_MEDIA_CONTENT_ID: IndexedMediaContentId = INDEXED_KEY_LOWER_UUID;

/// The maximum value for an [`IndexedMediaContentId`] - i.e.,
/// [`INDEXED_KEY_UPPER_UUID`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`IndexedMediaContentId`] values when used in conjunction with
/// [`INDEXED_KEY_LOWER_MEDIA_CONTENT_ID`].
pub const INDEXED_KEY_UPPER_MEDIA_CONTENT_ID: IndexedMediaContentId = INDEXED_KEY_UPPER_UUID;
