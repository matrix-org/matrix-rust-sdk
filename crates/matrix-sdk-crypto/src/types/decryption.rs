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

//! Data types for decrypting an event.

use serde::{Deserialize, Serialize};

/// The trust level required to decrypt an event.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub enum TrustRequirement {
    /// Decrypt events from everyone regardless of trust.
    Untrusted,
    /// Only decrypt events from cross-signed or legacy sessions (Megolm
    /// sessions created before we started collecting trust information).
    CrossSignedOrLegacy,
    /// Only decrypt events from cross-signed devices.
    CrossSigned,
}

/// Settings for decrypting messages
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DecryptionSettings {
    /// The trust level required to decrypt the event
    pub trust_requirement: TrustRequirement,
}
