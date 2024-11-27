// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use crate::types::CrossSigningKey;

/// Request that will publish a cross signing identity.
///
/// This uploads the public cross signing key triplet.
#[derive(Debug, Clone)]
pub struct UploadSigningKeysRequest {
    /// The user's master key.
    pub master_key: Option<CrossSigningKey>,
    /// The user's self-signing key. Must be signed with the accompanied master,
    /// or by the user's most recently uploaded master key if no master key
    /// is included in the request.
    pub self_signing_key: Option<CrossSigningKey>,
    /// The user's user-signing key. Must be signed with the accompanied master,
    /// or by the user's most recently uploaded master key if no master key
    /// is included in the request.
    pub user_signing_key: Option<CrossSigningKey>,
}
