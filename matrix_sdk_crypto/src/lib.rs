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

//! This is the encryption part of the matrix-sdk. It contains a state machine
//! that will aid in adding encryption support to a client library.

#![deny(
    missing_debug_implementations,
    dead_code,
    missing_docs,
    trivial_casts,
    trivial_numeric_casts,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications
)]
#![cfg_attr(feature = "docs", feature(doc_cfg))]

mod error;
mod file_encryption;
mod identities;
mod key_request;
mod machine;
pub mod olm;
mod requests;
mod session_manager;
pub mod store;
mod utilities;
mod verification;

pub use error::{MegolmError, OlmError};
pub use file_encryption::{
    decrypt_key_export, encrypt_key_export, AttachmentDecryptor, AttachmentEncryptor,
    DecryptorError, EncryptionInfo, KeyExportError,
};
pub use identities::{
    Device, LocalTrust, OwnUserIdentity, ReadOnlyDevice, UserDevices, UserIdentities, UserIdentity,
};
pub use machine::OlmMachine;
pub use matrix_qrcode;
pub use olm::EncryptionSettings;
pub(crate) use olm::ReadOnlyAccount;
pub use requests::{
    IncomingResponse, KeysQueryRequest, OutgoingRequest, OutgoingRequests,
    OutgoingVerificationRequest, RoomMessageRequest, ToDeviceRequest,
};
pub use store::CryptoStoreError;
pub use verification::{AcceptSettings, QrVerification, Sas, Verification, VerificationRequest};
