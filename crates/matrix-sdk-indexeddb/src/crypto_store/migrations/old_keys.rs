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

#[cfg(doc)]
use crate::crypto_store::keys::{BACKUP_VERSION_V1, WITHHELD_SESSIONS};

/// Old format of the `inbound_group_sessions` store which lacked indexes or
/// a sensible structure
pub const INBOUND_GROUP_SESSIONS_V1: &str = "inbound_group_sessions";

/// `inbound_group_sessions2` with large values in each record due to double
/// JSON-encoding and arrays of ints instead of base64.
/// Also lacked the `backed_up_to` property+index.
pub const INBOUND_GROUP_SESSIONS_V2: &str = "inbound_group_sessions2";

/// An old name for [`BACKUP_VERSION_V1`].
pub const BACKUP_KEY_V1: &str = "backup_key_v1";

/// Old format of the [`withheld_sessions`](WITHHELD_SESSIONS) store which
/// lacked useful indexes.
pub const DIRECT_WITHHELD_INFO: &str = "direct_withheld_info";
