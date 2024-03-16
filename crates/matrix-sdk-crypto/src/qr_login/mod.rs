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

#![allow(missing_docs, dead_code, clippy::todo)]

use thiserror::Error;
use vodozemac::secure_channel;

mod types;

pub use types::{QrCodeData, QrCodeDecodeError, QrCodeMode, QrCodeModeNum};

#[derive(Debug, Error)]
pub enum SecureChannelError {}

pub struct SecureChannel {
    inner: secure_channel::SecureChannel,
}

impl std::fmt::Debug for SecureChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecureChannel").finish_non_exhaustive()
    }
}

impl SecureChannel {
    pub fn qr_data(&self) -> QrCodeData {
        todo!()
    }

    pub fn from_qr_code(&self, _data: &QrCodeData) -> Result<Self, SecureChannelError> {
        todo!()
    }
}

pub struct EstablishedSecureChannel {
    inner: secure_channel::EstablishedSecureChannel,
}

impl std::fmt::Debug for EstablishedSecureChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EstablishedSecureChannel").finish_non_exhaustive()
    }
}

impl EstablishedSecureChannel {
    pub fn encrypt(&self, _message: &[u8]) -> Vec<u8> {
        todo!()
    }

    pub fn decrypt(&self) -> Result<Vec<u8>, SecureChannelError> {
        todo!()
    }
}
