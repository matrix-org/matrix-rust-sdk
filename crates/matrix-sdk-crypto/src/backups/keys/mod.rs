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

//! Module for the keys that are used to backup room keys.
//!
//! The backup key is split into two parts:
//!
//! ```text
//!                     ┌───────────────────────────┐
//!                     │  RecoveryKey | BackupKey  │
//!                     └───────────────────────────┘
//! ```
//!
//! 1. RecoveryKey, a private Curve25519 key that is used to decrypt backed up
//!    room keys.
//! 2. BackupKey, the public part of the Curve25519 RecoveryKey, this one is
//!    used to encrypt room keys that get backed up.
//!
//! The `RecoveryKey` can be derived from a passphrase or randomly generated.
//! To allow other devices access to this key you'll either have to re-enter the
//! passphrase or the key as a base58 encoded string or received from another
//! device using the secret sharing mechanism.
//!
//! In practice deriving a recovery key from a passphrase isn't done, and is
//! **not** supported by the spec. Instead the secret storage key that encrypts
//! secrets and puts them into your global account data gets derived from a
//! passphrase and presented as a base58 encoded string, confusingly also called
//! recovery key.
//!
//! The `RecoveryKey` can also be uploaded to your account data as an
//! `m.megolm.v1` event, which gets encrypted by the SSSS key.
//!
//! The `MegolmV1BackupKey` is used to encrypt individual room keys so they can
//! be uploaded to the homeserver.
//!
//! The `MegolmV1BackupKey` is a public key and is uploaded to the server using
//! the `/room_keys/version` API endpoint.

mod backup;
mod compat;
mod recovery;

pub use backup::MegolmV1BackupKey;
pub use compat::{Error as DecryptionError, MessageDecodeError};
pub use recovery::DecodeError;
