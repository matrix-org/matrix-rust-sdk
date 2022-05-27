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

use ruma::{signatures::CanonicalJsonError, IdParseError, OwnedDeviceId, OwnedRoomId, OwnedUserId};
use serde_json::Error as SerdeError;
use thiserror::Error;

use super::store::CryptoStoreError;

pub type OlmResult<T> = Result<T, OlmError>;
pub type MegolmResult<T> = Result<T, MegolmError>;

/// Error representing a failure during a device to device cryptographic
/// operation.
#[derive(Error, Debug)]
pub enum OlmError {
    /// The event that should have been decrypted is malformed.
    #[error(transparent)]
    EventError(#[from] EventError),

    /// The received decrypted event couldn't be deserialized.
    #[error(transparent)]
    JsonError(#[from] SerdeError),

    /// The event could not have been decrypted.
    #[error(transparent)]
    Decryption(#[from] vodozemac::olm::DecryptionError),

    /// The received room key couldn't be converted into a valid Megolm session.
    #[error(transparent)]
    SessionCreation(#[from] vodozemac::megolm::SessionKeyDecodeError),

    /// The storage layer returned an error.
    #[error("failed to read or write to the crypto store {0}")]
    Store(#[from] CryptoStoreError),

    /// The session with a device has become corrupted.
    #[error(
        "decryption failed likely because an Olm session from {0} with sender key {1} was wedged"
    )]
    SessionWedged(OwnedUserId, String),

    /// An Olm message got replayed while the Olm ratchet has already moved
    /// forward.
    #[error("decryption failed because an Olm message from {0} with sender key {1} was replayed")]
    ReplayedMessage(OwnedUserId, String),

    /// Encryption failed because the device does not have a valid Olm session
    /// with us.
    #[error(
        "encryption failed because the device does not \
            have a valid Olm session with us"
    )]
    MissingSession,
}

/// Error representing a failure during a group encryption operation.
#[derive(Error, Debug)]
pub enum MegolmError {
    /// The event that should have been decrypted is malformed.
    #[error(transparent)]
    EventError(#[from] EventError),

    /// The received decrypted event couldn't be deserialized.
    #[error(transparent)]
    JsonError(#[from] SerdeError),

    /// Decryption failed because we're missing the room key that was to encrypt
    /// the event.
    #[error("decryption failed because the room key is missing")]
    MissingRoomKey,

    /// The encrypted megolm message couldn't be decoded.
    #[error(transparent)]
    Decode(#[from] vodozemac::DecodeError),

    /// The room where a group session should be shared is not encrypted.
    #[error("The room where a group session should be shared is not encrypted")]
    EncryptionNotEnabled,

    /// The event could not have been decrypted.
    #[error(transparent)]
    Decryption(#[from] vodozemac::megolm::DecryptionError),

    /// The storage layer returned an error.
    #[error(transparent)]
    Store(#[from] CryptoStoreError),
}

#[derive(Error, Debug)]
pub enum EventError {
    #[error("the Olm message has a unsupported type, got {0}, expected 0 or 1")]
    UnsupportedOlmType(u64),

    #[error("the Encrypted message has been encrypted with a unsupported algorithm.")]
    UnsupportedAlgorithm,

    #[error("the provided JSON value isn't an object")]
    NotAnObject,

    #[error("the Encrypted message doesn't contain a ciphertext for our device")]
    MissingCiphertext,

    #[error("the Encrypted message is missing the signing key of the sender")]
    MissingSigningKey,

    #[error("the Encrypted message is missing the sender key")]
    MissingSenderKey,

    #[error("the Encrypted message is missing the field {0}")]
    MissingField(String),

    #[error(
        "the sender of the plaintext doesn't match the sender of the encrypted \
        message, got {0}, expected {0}"
    )]
    MismatchedSender(OwnedUserId, OwnedUserId),

    #[error(
        "the public that was part of the message doesn't match to the key we \
        have stored, expected {0}, got {0}"
    )]
    MismatchedKeys(String, String),

    #[error(
        "the room id of the room key doesn't match the room id of the \
        decrypted event: expected {0}, got {:1}"
    )]
    MismatchedRoom(OwnedRoomId, Option<OwnedRoomId>),
}

/// Error type describin different errors that happen when we check or create
/// signatures for a Matrix JSON object.
#[derive(Error, Debug)]
pub enum SignatureError {
    /// The signature was made using an unsupported algorithm.
    #[error("the signature used an unsupported algorithm")]
    UnsupportedAlgorithm,

    /// The ID of the signing key isn't a valid key ID.
    #[error("the ID of the signing key is invalid")]
    InvalidKeyId(#[from] IdParseError),

    /// The signing key that should create or check a signature is missing.
    #[error("the signing key is missing from the object that signed the message")]
    MissingSigningKey,

    /// The user id of signing key differs from the user id that provided the
    /// signature.
    #[error("the user id of the signing key differs user id that provided the signature")]
    UserIdMismatch,

    /// The provided JSON value that was signed and the signature should be
    /// checked isn't a valid JSON object.
    #[error("the provided JSON value isn't an object")]
    NotAnObject,

    /// The provided JSON value that was signed and the signature should be
    /// checked isn't a valid JSON object.
    #[error("the provided JSON object doesn't contain a signatures field")]
    NoSignatureFound,

    /// The signature couldn't be verified.
    #[error(transparent)]
    VerificationError(#[from] vodozemac::SignatureError),

    /// The public key isn't a valid ed25519 key.
    #[error(transparent)]
    InvalidKey(#[from] vodozemac::KeyError),

    /// The signed object couldn't be deserialized.
    #[error(transparent)]
    JsonError(#[from] CanonicalJsonError),
}

impl From<SerdeError> for SignatureError {
    fn from(e: SerdeError) -> Self {
        CanonicalJsonError::SerDe(e).into()
    }
}

#[derive(Error, Debug)]
pub enum SessionCreationError {
    #[error(
        "Failed to create a new Olm session for {0} {1}, the requested \
        one-time key isn't a signed curve key"
    )]
    OneTimeKeyNotSigned(OwnedUserId, OwnedDeviceId),
    #[error(
        "Tried to create a new Olm session for {0} {1}, but the signed \
        one-time key is missing"
    )]
    OneTimeKeyMissing(OwnedUserId, OwnedDeviceId),
    #[error(
        "Tried to create a new Olm session for {0} {1}, but the one-time \
        key algorithm is unsupported"
    )]
    OneTimeKeyUnknown(OwnedUserId, OwnedDeviceId),
    #[error("Failed to verify the one-time key signatures for {0} {1}: {2:?}")]
    InvalidSignature(OwnedUserId, OwnedDeviceId, SignatureError),
    #[error(
        "Tried to create an Olm session for {0} {1}, but the device is missing \
        a curve25519 key"
    )]
    DeviceMissingCurveKey(OwnedUserId, OwnedDeviceId),
    #[error("Error deserializing the one-time key: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("The given curve25519 key is not a valid key")]
    InvalidCurveKey(#[from] vodozemac::KeyError),
    #[error(transparent)]
    InboundCreation(#[from] vodozemac::olm::SessionCreationError),
}
