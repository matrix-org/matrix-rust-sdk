// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! Structs and keys used for reading/writing objects in schema v7

/// The objects we store in the inbound_group_sessions2 indexeddb object
/// store (in schemas v7 and v8)
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct InboundGroupSessionIndexedDbObject2 {
    /// (Possibly encrypted) serialisation of a
    /// [`matrix_sdk_crypto::olm::group_sessions::PickledInboundGroupSession`]
    /// structure.
    pub pickled_session: Vec<u8>,

    /// Whether the session data has yet to be backed up.
    ///
    /// Since we only need to be able to find entries where this is `true`,
    /// we skip serialization in cases where it is `false`. That has
    /// the effect of omitting it from the indexeddb index.
    ///
    /// We also use a custom serializer because bools can't be used as keys
    /// in indexeddb.
    #[serde(
        default,
        skip_serializing_if = "std::ops::Not::not",
        with = "crate::serializer::foreign::bool"
    )]
    pub needs_backup: bool,
}
