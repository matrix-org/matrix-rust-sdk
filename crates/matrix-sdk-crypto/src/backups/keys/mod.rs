// Copyright 2021 The Matrix.org Foundation C.I.C.
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

//! Module for the keys that are used to back up room keys.
//!
//! The backup key is split into two parts:
//!
//! ```text
//!                 ┌───────────────────────────────────────────┐
//!                 │  BackupDecryptionKey | MegolmV1BackupKey  │
//!                 └───────────────────────────────────────────┘
//! ```
//!
//! 1. [`crate::store::BackupDecryptionKey`], a private Curve25519 key that is
//!    used to decrypt backed up room keys. Sometimes also referred to as a
//!    "recovery key".
//!
//! 2. [`MegolmV1BackupKey`], the public part of the Curve25519
//!    `BackupDecryptionKey`. This is used to encrypt room keys that get backed
//!    up.
//!
//! In theory, the `BackupDecryptionKey` can be derived from a passphrase.
//! However, in practice deriving a decryption key from a passphrase isn't done,
//! and is **not** supported by the spec.
//!
//! Instead, it is randomly generated, and then encrypted using the server-side
//! secret storage (SSSS) key. (The SSSS key is, confusingly, also called a
//! "recovery key".)
//!
//! The (encrypted) `BackupDecryptionKey` can then be uploaded to your account
//! data as an `m.megolm.v1` event.
//!
//! The `MegolmV1BackupKey` is used to encrypt individual room keys so they can
//! be uploaded to the homeserver.
//!
//! The `MegolmV1BackupKey` is a public key and is uploaded to the server using
//! the `/room_keys/version` API endpoint.

mod backup;
mod compat;
mod decryption;

pub use backup::MegolmV1BackupKey;
pub use compat::{Error as DecryptionError, MessageDecodeError};
pub use decryption::DecodeError;
