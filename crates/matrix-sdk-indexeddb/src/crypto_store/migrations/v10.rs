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

//! Structs and keys used for reading/writing objects in schema v10

use crate::crypto_store::indexeddb_serializer::MaybeEncrypted;

/// The objects we store in the inbound_group_sessions3 indexeddb object
/// store (in schema v10)
#[derive(serde::Serialize, serde::Deserialize)]
pub struct InboundGroupSessionIndexedDbObject3 {
    /// Possibly encrypted
    /// [`matrix_sdk_crypto::olm::group_sessions::PickledInboundGroupSession`]
    pickled_session: MaybeEncrypted,

    /// Whether the session data has yet to be backed up.
    ///
    /// Since we only need to be able to find entries where this is `true`, we
    /// skip serialization in cases where it is `false`. That has the effect
    /// of omitting it from the indexeddb index.
    ///
    /// We also use a custom serializer because bools can't be used as keys in
    /// indexeddb.
    #[serde(
        default,
        skip_serializing_if = "std::ops::Not::not",
        with = "crate::serialize_bool_for_indexeddb"
    )]
    needs_backup: bool,

    /// Unused: for future compatibility. In future, will contain the order
    /// number (not the ID!) of the backup for which this key has been
    /// backed up. This will replace `needs_backup`, fixing the performance
    /// problem identified in
    /// https://github.com/element-hq/element-web/issues/26892
    /// because we won't need to update all records when we spot a new backup
    /// version.
    /// In this version of the code, this is always set to -1, meaning:
    /// "refer to the `needs_backup` property". See:
    /// https://github.com/element-hq/element-web/issues/26892#issuecomment-1906336076
    backed_up_to: i32,
}

impl InboundGroupSessionIndexedDbObject3 {
    pub fn new(pickled_session: MaybeEncrypted, needs_backup: bool) -> Self {
        Self { pickled_session, needs_backup, backed_up_to: -1 }
    }
}
