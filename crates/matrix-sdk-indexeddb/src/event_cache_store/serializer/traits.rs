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
