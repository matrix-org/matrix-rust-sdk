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

use crate::serializer::{
    indexed_type::traits::{Indexed, IndexedKey, IndexedKeyBounds, IndexedPrefixKeyBounds},
    safe_encode::types::SafeEncodeSerializer,
};

/// Representation of a range of keys of type `K`. This is loosely
/// correlated with [IDBKeyRange][1], with a few differences.
///
/// Namely, this enum only provides a single way to express a bounded range
/// which is always inclusive on both bounds. While all ranges can still be
/// represented, [`IDBKeyRange`][1] provides more flexibility in this regard.
///
/// [1]: https://developer.mozilla.org/en-US/docs/Web/API/IDBKeyRange
#[derive(Debug, Copy, Clone)]
pub enum IndexedKeyRange<K> {
    /// Represents a single key of type `K`.
    ///
    /// Identical to [`IDBKeyRange.only`][1].
    ///
    /// [1]: https://developer.mozilla.org/en-US/docs/Web/API/IDBKeyRange/only
    Only(K),
    /// Represents an inclusive range of keys of type `K`
    /// where the first item is the lower bound and the
    /// second item is the upper bound.
    ///
    /// Similar to [`IDBKeyRange.bound`][1].
    ///
    /// [1]: https://developer.mozilla.org/en-US/docs/Web/API/IDBKeyRange/bound
    Bound(K, K),
}

impl<'a, C: 'a> IndexedKeyRange<C> {
    /// Encodes a range of key components of type `K::KeyComponents`
    /// into a range of keys of type `K`.
    pub fn encoded<T, K>(self, serializer: &SafeEncodeSerializer) -> IndexedKeyRange<K>
    where
        T: Indexed,
        K: IndexedKey<T, KeyComponents<'a> = C>,
    {
        match self {
            Self::Only(components) => IndexedKeyRange::Only(K::encode(components, serializer)),
            Self::Bound(lower, upper) => {
                IndexedKeyRange::Bound(K::encode(lower, serializer), K::encode(upper, serializer))
            }
        }
    }
}

impl<K> IndexedKeyRange<K> {
    /// Applies the function `f` to transform keys of type `K` to keys of type
    /// `T`. The function `f` is applied to all values - i.e., the value in
    /// [`IndexedKeyRange::Only`] and both bounds in [`IndexedKeyRange::Bound`].
    pub fn map<T, F>(self, f: F) -> IndexedKeyRange<T>
    where
        F: Fn(K) -> T,
    {
        match self {
            IndexedKeyRange::Only(key) => IndexedKeyRange::Only(f(key)),
            IndexedKeyRange::Bound(lower, upper) => IndexedKeyRange::Bound(f(lower), f(upper)),
        }
    }

    /// Represents all keys of type `K`
    pub fn all<T>(serializer: &SafeEncodeSerializer) -> IndexedKeyRange<K>
    where
        T: Indexed,
        K: IndexedKeyBounds<T>,
    {
        IndexedKeyRange::Bound(K::lower_key(serializer), K::upper_key(serializer))
    }

    /// Represents all keys of type `K` which begin with `prefix`
    pub fn all_with_prefix<T, P>(prefix: P, serializer: &SafeEncodeSerializer) -> IndexedKeyRange<K>
    where
        T: Indexed,
        K: IndexedPrefixKeyBounds<T, P>,
        P: Clone,
    {
        IndexedKeyRange::Bound(
            K::lower_key_with_prefix(prefix.clone(), serializer),
            K::upper_key_with_prefix(prefix, serializer),
        )
    }

    /// Transforms keys of type `K` into keys of type `K2` by using `K` as the
    /// prefix for `K2`.
    ///
    /// For [`IndexedKeyRange::Only`] variants, the resulting value will be an
    /// [`IndexedKeyRange::Bound`] that represents all keys that begin with
    /// [`IndexedKeyRange::Only::0`].
    ///
    /// For [`IndexedKeyRange::Bound`] variants, the resulting value will be an
    /// [`IndexedKeyRange::Bound`] that represents all keys in a range where
    /// the lower bound begins with [`IndexedKeyRange::Bound::0`] and the upper
    /// bound begins with [`IndexedKeyRange::Bound::1`].
    pub fn into_prefix<T, K2>(self, serializer: &SafeEncodeSerializer) -> IndexedKeyRange<K2>
    where
        T: Indexed,
        K: Clone,
        K2: IndexedPrefixKeyBounds<T, K>,
    {
        match self {
            IndexedKeyRange::Only(key) => IndexedKeyRange::all_with_prefix(key, serializer),
            IndexedKeyRange::Bound(lower, upper) => IndexedKeyRange::Bound(
                K2::lower_key_with_prefix(lower, serializer),
                K2::upper_key_with_prefix(upper, serializer),
            ),
        }
    }
}

impl<K> From<(K, K)> for IndexedKeyRange<K> {
    fn from(value: (K, K)) -> Self {
        Self::Bound(value.0, value.1)
    }
}

impl<K> From<K> for IndexedKeyRange<K> {
    fn from(value: K) -> Self {
        Self::Only(value)
    }
}
