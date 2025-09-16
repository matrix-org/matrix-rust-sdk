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

#[cfg(feature = "e2e-encryption")]
pub mod foreign;

#[cfg(feature = "e2e-encryption")]
pub mod indexed_type;
#[cfg(feature = "e2e-encryption")]
pub use indexed_type::{
    constants::{
        INDEXED_KEY_LOWER_CHARACTER, INDEXED_KEY_LOWER_DURATION, INDEXED_KEY_LOWER_STRING,
        INDEXED_KEY_UPPER_CHARACTER, INDEXED_KEY_UPPER_DURATION_SECONDS, INDEXED_KEY_UPPER_STRING,
    },
    range::IndexedKeyRange,
    traits::{
        Indexed, IndexedKey, IndexedKeyBounds, IndexedKeyComponentBounds, IndexedPrefixKeyBounds,
        IndexedPrefixKeyComponentBounds,
    },
    IndexedTypeSerializer,
};

pub mod safe_encode;
#[cfg(feature = "e2e-encryption")]
pub use safe_encode::types::{MaybeEncrypted, SafeEncodeSerializer, SafeEncodeSerializerError};
