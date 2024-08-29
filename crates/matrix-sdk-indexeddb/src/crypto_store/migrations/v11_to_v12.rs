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

//! Migration code that moves from inbound_group_sessions2 to
//! inbound_group_sessions3, shrinking the values stored in each record.

use indexed_db_futures::IdbKeyPath;
use web_sys::DomException;

use crate::crypto_store::{keys, migrations::do_schema_upgrade, Result};

/// Perform the schema upgrade v11 to v12, adding an index on
/// `(curve_key, sender_data_type, session_id)` to `inbound_group_sessions3`.
pub(crate) async fn schema_add(name: &str) -> Result<(), DomException> {
    do_schema_upgrade(name, 12, |_, transaction, _| {
        let object_store = transaction.object_store(keys::INBOUND_GROUP_SESSIONS_V3)?;

        object_store.create_index(
            keys::INBOUND_GROUP_SESSIONS_SENDER_KEY_INDEX,
            &IdbKeyPath::str_sequence(&["sender_key", "sender_data_type", "session_id"]),
        )?;

        Ok(())
    })
    .await
}
