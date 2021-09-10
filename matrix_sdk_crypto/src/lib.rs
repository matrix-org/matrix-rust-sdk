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

#![doc = include_str!("../README.md")]
#![cfg_attr(feature = "docs", feature(doc_cfg))]
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

mod error;
mod file_encryption;
mod gossiping;
mod identities;
mod machine;
pub mod olm;
mod requests;
mod session_manager;
pub mod store;
mod utilities;
mod verification;

pub use error::{MegolmError, OlmError, SignatureError};
pub use file_encryption::{
    decrypt_key_export, encrypt_key_export, AttachmentDecryptor, AttachmentEncryptor,
    DecryptorError, EncryptionInfo, KeyExportError,
};
pub use identities::{
    Device, LocalTrust, MasterPubkey, OwnUserIdentity, ReadOnlyDevice, ReadOnlyOwnUserIdentity,
    ReadOnlyUserIdentities, ReadOnlyUserIdentity, UserDevices, UserIdentities, UserIdentity,
};
pub use machine::OlmMachine;
#[cfg(feature = "qrcode")]
pub use matrix_qrcode;
pub(crate) use olm::ReadOnlyAccount;
pub use olm::{CrossSigningStatus, EncryptionSettings};
pub use requests::{
    IncomingResponse, KeysQueryRequest, OutgoingRequest, OutgoingRequests,
    OutgoingVerificationRequest, RoomMessageRequest, ToDeviceRequest, UploadSigningKeysRequest,
};
pub use store::{CrossSigningKeyExport, CryptoStoreError, SecretImportError};
#[cfg(feature = "qrcode")]
pub use verification::QrVerification;
pub use verification::{AcceptSettings, CancelInfo, Sas, Verification, VerificationRequest};
