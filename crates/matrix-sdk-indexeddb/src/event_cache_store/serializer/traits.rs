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

use ruma::RoomId;

use crate::serializer::IndexeddbSerializer;

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
        room_id: &RoomId,
        serializer: &IndexeddbSerializer,
    ) -> Result<Self::IndexedType, Self::Error>;

    /// Converts an indexed type into the high-level type.
    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &IndexeddbSerializer,
    ) -> Result<Self, Self::Error>;
}

/// A trait for encoding types which will be used as keys in IndexedDB.
///
/// Each implementation represents a key on an [`Indexed`] type.
pub trait IndexedKey<T: Indexed> {
    /// The index name for the key, if it represents an index.
    const INDEX: Option<&'static str> = None;

    /// Any extra data used to construct the key.
    type KeyComponents;

    /// Encodes the key components into a type that can be used as a key in
    /// IndexedDB.
    ///
    /// Note that this function takes an [`IndexeddbSerializer`] as an
    /// argument, which provides the necessary context for encryption and
    /// decryption, in the case that certain components of the key must be
    /// encrypted before storage.
    fn encode(
        room_id: &RoomId,
        components: &Self::KeyComponents,
        serializer: &IndexeddbSerializer,
    ) -> Self;
}

/// A trait for constructing the bounds of an [`IndexedKey`].
///
/// This is useful when constructing range queries in IndexedDB.
pub trait IndexedKeyBounds<T: Indexed>: IndexedKey<T> {
    /// Constructs the lower bound of the key.
    fn lower_key(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self;

    /// Constructs the upper bound of the key.
    fn upper_key(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self;
}

impl<T, K> IndexedKeyBounds<T> for K
where
    T: Indexed,
    K: IndexedKeyComponentBounds<T> + Sized,
{
    /// Constructs the lower bound of the key.
    fn lower_key(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        <Self as IndexedKey<T>>::encode(room_id, &Self::lower_key_components(), serializer)
    }

    /// Constructs the upper bound of the key.
    fn upper_key(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        <Self as IndexedKey<T>>::encode(room_id, &Self::upper_key_components(), serializer)
    }
}

/// A trait for constructing the bounds of the components of an [`IndexedKey`].
///
/// This is useful when constructing range queries in IndexedDB. Note that this
/// trait should not be implemented for key components that are going to be
/// encrypted as ordering properties will not be preserved.
pub trait IndexedKeyComponentBounds<T: Indexed>: IndexedKeyBounds<T> {
    /// Constructs the lower bound of the key components.
    fn lower_key_components() -> Self::KeyComponents;

    /// Constructs the upper bound of the key components.
    fn upper_key_components() -> Self::KeyComponents;
}
