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
// limitations under the License.

use matrix_sdk_crypto::{DecryptionSettings, OlmMachine};

pub mod decrypt;
pub mod to_device;
pub mod tracked_users;

/// A classical set of data used by some processors in this module.
#[derive(Clone)]
pub struct E2EE<'a> {
    pub olm_machine: Option<&'a OlmMachine>,
    pub decryption_settings: &'a DecryptionSettings,
    pub verification_is_allowed: bool,
}

impl<'a> E2EE<'a> {
    pub fn new(
        olm_machine: Option<&'a OlmMachine>,
        decryption_settings: &'a DecryptionSettings,
        verification_is_allowed: bool,
    ) -> Self {
        Self { olm_machine, decryption_settings, verification_is_allowed }
    }
}
