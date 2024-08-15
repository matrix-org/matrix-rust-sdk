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
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#![warn(missing_docs, missing_debug_implementations)]

pub mod backups;
mod ciphers;
pub mod dehydrated_devices;
mod error;
mod file_encryption;
mod gossiping;
mod identities;
mod machine;
pub mod olm;
pub mod requests;
pub mod secret_storage;
mod session_manager;
pub mod store;
pub mod types;
mod utilities;
mod verification;

#[cfg(any(test, feature = "testing"))]
/// Testing facilities and helpers for crypto tests
pub mod testing {
    pub use crate::identities::{
        device::testing::get_device,
        user::testing::{get_other_identity, get_own_identity},
    };
}

use std::collections::{BTreeMap, BTreeSet};

use ruma::OwnedRoomId;

/// Return type for the room key importing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomKeyImportResult {
    /// The number of room keys that were imported.
    pub imported_count: usize,
    /// The total number of room keys that were found in the export.
    pub total_count: usize,
    /// The map of keys that were imported.
    ///
    /// It's a map from room id to a map of the sender key to a set of session
    /// ids.
    pub keys: BTreeMap<OwnedRoomId, BTreeMap<String, BTreeSet<String>>>,
}

impl RoomKeyImportResult {
    pub(crate) fn new(
        imported_count: usize,
        total_count: usize,
        keys: BTreeMap<OwnedRoomId, BTreeMap<String, BTreeSet<String>>>,
    ) -> Self {
        Self { imported_count, total_count, keys }
    }
}

pub use error::{
    EventError, MegolmError, OlmError, SessionCreationError, SessionRecipientCollectionError,
    SetRoomSettingsError, SignatureError,
};
pub use file_encryption::{
    decrypt_room_key_export, encrypt_room_key_export, AttachmentDecryptor, AttachmentEncryptor,
    DecryptorError, KeyExportError, MediaEncryptionInfo,
};
pub use gossiping::{GossipRequest, GossippedSecret};
pub use identities::{
    Device, DeviceData, LocalTrust, OtherUserIdentityData, OwnUserIdentity, OwnUserIdentityData,
    UserDevices, UserIdentities, UserIdentity, UserIdentityData,
};
pub use machine::{CrossSigningBootstrapRequests, EncryptionSyncChanges, OlmMachine};
#[cfg(feature = "qrcode")]
pub use matrix_sdk_qrcode;
pub use olm::{Account, CrossSigningStatus, EncryptionSettings, Session};
pub use requests::{
    IncomingResponse, KeysBackupRequest, KeysQueryRequest, OutgoingRequest, OutgoingRequests,
    OutgoingVerificationRequest, RoomMessageRequest, ToDeviceRequest, UploadSigningKeysRequest,
};
pub use session_manager::CollectStrategy;
pub use store::{
    CrossSigningKeyExport, CryptoStoreError, SecretImportError, SecretInfo, TrackedUser,
};
pub use verification::{
    format_emojis, AcceptSettings, AcceptedProtocols, CancelInfo, Emoji, EmojiShortAuthString, Sas,
    SasState, Verification, VerificationRequest, VerificationRequestState,
};
#[cfg(feature = "qrcode")]
pub use verification::{QrVerification, QrVerificationState, ScanError};
#[doc(no_inline)]
pub use vodozemac;

/// The version of the matrix-sdk-cypto crate being used
pub static VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
matrix_sdk_test::init_tracing_for_tests!();

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();
