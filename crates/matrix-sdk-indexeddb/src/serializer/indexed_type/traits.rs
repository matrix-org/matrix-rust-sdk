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

use crate::serializer::safe_encode::types::SafeEncodeSerializer;

/// A conversion trait for preparing high-level types into indexed types
/// which are better suited for storage in IndexedDB.
///
/// Note that the functions below take an [`IndexeddbSerializer`] as an
/// argument, which provides the necessary context for encryption and
/// decryption, in the case the high-level type must be encrypted before
/// storage.
pub trait Indexed: Sized {
    /// The name of the object store in IndexedDB.
    const OBJECT_STORE: &'static str;

    /// The indexed type that is used for storage in IndexedDB.
    type IndexedType;
    /// The error type that is returned when conversion fails.
    type Error;

    /// Converts the high-level type into an indexed type.
    fn to_indexed(
        &self,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self::IndexedType, Self::Error>;

    /// Converts an indexed type into the high-level type.
    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self, Self::Error>;
}

/// A trait for encoding types which will be used as keys in IndexedDB.
///
/// Each implementation represents a key on an [`Indexed`] type.
pub trait IndexedKey<T: Indexed> {
    /// The index name for the key, if it represents an index.
    const INDEX: Option<&'static str> = None;

    /// Any extra data used to construct the key.
    type KeyComponents<'a>;

    /// Encodes the key components into a type that can be used as a key in
    /// IndexedDB.
    ///
    /// Note that this function takes an [`IndexeddbSerializer`] as an
    /// argument, which provides the necessary context for encryption and
    /// decryption, in the case that certain components of the key must be
    /// encrypted before storage.
    fn encode(components: Self::KeyComponents<'_>, serializer: &SafeEncodeSerializer) -> Self;
}

/// A trait for constructing the bounds of an [`IndexedKey`].
///
/// This is useful when constructing range queries in IndexedDB.
///
/// The [`IndexedKeyComponentBounds`] helps to specify the upper and lower
/// bounds of the components that are used to create the final key, while the
/// `IndexedKeyBounds` are the upper and lower bounds of the final key itself.
///
/// While these concepts are similar and often produce the same results, there
/// are cases where these two concepts produce very different results. Namely,
/// when any of the components are encrypted in the process of constructing the
/// final key, then the component bounds and the key bounds produce very
/// different results.
///
/// So, for instance, consider the `EventId`, which may be encrypted before
/// being used in a final key. One cannot construct the upper and lower bounds
/// of the final key using the upper and lower bounds of the `EventId`, because
/// once the `EventId` is encrypted, the resultant value will no longer express
/// the proper bound.
pub trait IndexedKeyBounds<T: Indexed>: IndexedKey<T> {
    /// Constructs the lower bound of the key.
    fn lower_key(serializer: &SafeEncodeSerializer) -> Self;

    /// Constructs the upper bound of the key.
    fn upper_key(serializer: &SafeEncodeSerializer) -> Self;
}

impl<T, K> IndexedKeyBounds<T> for K
where
    T: Indexed,
    K: IndexedKeyComponentBounds<T> + Sized,
{
    /// Constructs the lower bound of the key.
    fn lower_key(serializer: &SafeEncodeSerializer) -> Self {
        <Self as IndexedKey<T>>::encode(Self::lower_key_components(), serializer)
    }

    /// Constructs the upper bound of the key.
    fn upper_key(serializer: &SafeEncodeSerializer) -> Self {
        <Self as IndexedKey<T>>::encode(Self::upper_key_components(), serializer)
    }
}

/// A trait for constructing the bounds of the components of an [`IndexedKey`].
///
/// This is useful when constructing range queries in IndexedDB. Note that this
/// trait should not be implemented for key components that are going to be
/// encrypted as ordering properties will not be preserved.
///
/// One may be interested to read the documentation of [`IndexedKeyBounds`] to
/// get a better overview of how these two interact.
pub trait IndexedKeyComponentBounds<T: Indexed>: IndexedKeyBounds<T> {
    /// Constructs the lower bound of the key components.
    fn lower_key_components() -> Self::KeyComponents<'static>;

    /// Constructs the upper bound of the key components.
    fn upper_key_components() -> Self::KeyComponents<'static>;
}

/// A trait for constructing the bounds of an [`IndexedKey`] given a prefix `P`
/// of that key.
///
/// The key bounds should be constructed by keeping the prefix constant while
/// the remaining components of the key are set to their lower and upper limits.
///
/// This is useful when constructing prefixed range queries in IndexedDB.
///
/// Note that the [`IndexedPrefixKeyComponentBounds`] helps to specify the upper
/// and lower bounds of the components that are used to create the final key,
/// while the `IndexedPrefixKeyBounds` are the upper and lower bounds of the
/// final key itself.
///
/// For details on the differences between key bounds and key component bounds,
/// see the documentation on [`IndexedKeyBounds`].
pub trait IndexedPrefixKeyBounds<T: Indexed, P>: IndexedKey<T> {
    /// Constructs the lower bound of the key while maintaining a constant
    /// prefix.
    fn lower_key_with_prefix(prefix: P, serializer: &SafeEncodeSerializer) -> Self;

    /// Constructs the upper bound of the key while maintaining a constant
    /// prefix.
    fn upper_key_with_prefix(prefix: P, serializer: &SafeEncodeSerializer) -> Self;
}

impl<'a, T, K, P> IndexedPrefixKeyBounds<T, P> for K
where
    T: Indexed,
    K: IndexedPrefixKeyComponentBounds<'a, T, P> + Sized,
    P: 'a,
{
    fn lower_key_with_prefix(prefix: P, serializer: &SafeEncodeSerializer) -> Self {
        <Self as IndexedKey<T>>::encode(Self::lower_key_components_with_prefix(prefix), serializer)
    }

    fn upper_key_with_prefix(prefix: P, serializer: &SafeEncodeSerializer) -> Self {
        <Self as IndexedKey<T>>::encode(Self::upper_key_components_with_prefix(prefix), serializer)
    }
}

/// A trait for constructing the bounds of the components of an [`IndexedKey`]
/// given a prefix `P` of that key.
///
/// The key component bounds should be constructed by keeping the prefix
/// constant while the remaining components of the key are set to their lower
/// and upper limits.
///
/// This is useful when constructing range queries in IndexedDB.
///
/// Note that this trait should not be implemented for key components that are
/// going to be encrypted as ordering properties will not be preserved.
///
/// One may be interested to read the documentation of [`IndexedKeyBounds`] to
/// get a better overview of how these two interact.
pub trait IndexedPrefixKeyComponentBounds<'a, T: Indexed, P: 'a>: IndexedKey<T> {
    /// Constructs the lower bound of the key components while maintaining a
    /// constant prefix.
    fn lower_key_components_with_prefix(prefix: P) -> Self::KeyComponents<'a>;

    /// Constructs the upper bound of the key components while maintaining a
    /// constant prefix.
    fn upper_key_components_with_prefix(prefix: P) -> Self::KeyComponents<'a>;
}
