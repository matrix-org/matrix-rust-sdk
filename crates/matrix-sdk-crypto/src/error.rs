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

use std::collections::BTreeMap;

use matrix_sdk_common::deserialized_responses::{VerificationLevel, WithheldCode};
use ruma::{CanonicalJsonError, IdParseError, OwnedDeviceId, OwnedRoomId, OwnedUserId};
use serde::{Serializer, ser::SerializeMap};
use serde_json::Error as SerdeError;
use thiserror::Error;
use vodozemac::{Curve25519PublicKey, Ed25519PublicKey};

use super::store::CryptoStoreError;
#[cfg(doc)]
use crate::{CollectStrategy, Device, LocalTrust, OtherUserIdentity};
use crate::{olm::SessionExportError, types::SignedKey};

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

    /// The received room key couldn't be converted into a valid Megolm session.
    #[error(transparent)]
    SessionCreation(#[from] SessionCreationError),

    /// The room key that should be exported can't be converted into a
    /// `m.forwarded_room_key` event.
    #[error(transparent)]
    SessionExport(#[from] SessionExportError),

    /// The storage layer returned an error.
    #[error("failed to read or write to the crypto store {0}")]
    Store(#[from] CryptoStoreError),

    /// The session with a device has become corrupted.
    #[error(
        "decryption failed likely because an Olm session from {0} with sender key {1} was wedged"
    )]
    SessionWedged(OwnedUserId, Curve25519PublicKey),

    /// An Olm message got replayed while the Olm ratchet has already moved
    /// forward.
    #[error("decryption failed because an Olm message from {0} with sender key {1} was replayed")]
    ReplayedMessage(OwnedUserId, Curve25519PublicKey),

    /// Encryption failed because the device does not have a valid Olm session
    /// with us.
    #[error(
        "encryption failed because the device does not \
            have a valid Olm session with us"
    )]
    MissingSession,

    /// Encryption failed due to an error collecting the recipient devices.
    #[error("encryption failed due to an error collecting the recipient devices: {0}")]
    SessionRecipientCollectionError(SessionRecipientCollectionError),

    /// Encrypted content is withheld from this device
    #[error("encryption content is withheld from this: {0}")]
    Withheld(WithheldCode),

    /// Refused to decrypt because the sender was not verified or did not meet
    /// the required VerificationLevel.
    #[error(
        "refusing to decrypt the event because the sender device was not \
        verified and 'exclude insecure devices' is enabled."
    )]
    UnverifiedSenderDevice,
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

    /// Decryption failed because we're missing the room key that was used to
    /// encrypt the event.
    #[error("Can't find the room key to decrypt the event, withheld code: {0:?}")]
    MissingRoomKey(Option<WithheldCode>),

    /// Decryption failed because of a mismatch between the identity keys of the
    /// device we received the room key from and the identity keys recorded in
    /// the plaintext of the room key to-device message.
    #[error(
        "decryption failed because of mismatched identity keys of the sending device and those recorded in the to-device message"
    )]
    MismatchedIdentityKeys(MismatchedIdentityKeysError),

    /// The encrypted megolm message couldn't be decoded.
    #[error(transparent)]
    Decode(#[from] vodozemac::DecodeError),

    /// The event could not have been decrypted.
    #[error(transparent)]
    Decryption(#[from] vodozemac::megolm::DecryptionError),

    /// The storage layer returned an error.
    #[error(transparent)]
    Store(#[from] CryptoStoreError),

    /// An encrypted message wasn't decrypted, because the sender's
    /// cross-signing identity did not satisfy the requested
    /// [`crate::TrustRequirement`].
    ///
    /// The nested value is the sender's current verification level.
    #[error("decryption failed because trust requirement not satisfied: {0}")]
    SenderIdentityNotTrusted(VerificationLevel),

    /// The outer state key could not be verified against the inner encrypted
    /// state key and type.
    #[cfg(feature = "experimental-encrypted-state-events")]
    #[error("decryption failed because the state key failed to validate")]
    StateKeyVerificationFailed,
}

/// Decryption failed because of a mismatch between the identity keys of the
/// device we received the room key from and the identity keys recorded in
/// the plaintext of the room key to-device message.
#[derive(Error, Debug, PartialEq)]
pub struct MismatchedIdentityKeysError {
    /// The Ed25519 key recorded in the room key's to-device message.
    pub key_ed25519: Box<Ed25519PublicKey>,
    /// The Ed25519 identity key of the device sending the room key.
    pub device_ed25519: Option<Box<Ed25519PublicKey>>,
    /// The Curve25519 key recorded in the room key's to-device message.
    pub key_curve25519: Box<Curve25519PublicKey>,
    /// The Curve25519 identity key of the device sending the room key.
    pub device_curve25519: Option<Box<Curve25519PublicKey>>,
}

impl std::fmt::Display for MismatchedIdentityKeysError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut ser = f.serialize_struct("MismatchedIdentityKeysError", 4)?;
        ser.serialize_entry("key_ed25519", &self.key_ed25519)?;
        ser.serialize_entry("device_ed25519", &self.device_ed25519)?;
        ser.serialize_entry("key_curve25519", &self.key_curve25519)?;
        ser.serialize_entry("device_curve25519", &self.device_curve25519)?;
        ser.end()
    }
}

impl From<MismatchedIdentityKeysError> for MegolmError {
    fn from(value: MismatchedIdentityKeysError) -> Self {
        MegolmError::MismatchedIdentityKeys(value)
    }
}

impl From<MismatchedIdentityKeysError> for SessionCreationError {
    fn from(value: MismatchedIdentityKeysError) -> Self {
        SessionCreationError::MismatchedIdentityKeys(value)
    }
}

/// Error that occurs when decrypting an event that is malformed.
#[derive(Error, Debug)]
pub enum EventError {
    /// The Encrypted message has been encrypted with a unsupported algorithm.
    #[error("the Encrypted message has been encrypted with a unsupported algorithm.")]
    UnsupportedAlgorithm,

    /// The provided JSON value isn't an object.
    #[error("the provided JSON value isn't an object")]
    NotAnObject,

    /// The Encrypted message doesn't contain a ciphertext for our device.
    #[error("the Encrypted message doesn't contain a ciphertext for our device")]
    MissingCiphertext,

    /// The Encrypted message is missing the signing key of the sender.
    #[error("the Encrypted message is missing the signing key of the sender")]
    MissingSigningKey,

    /// The Encrypted message is missing the sender key.
    #[error("the Encrypted message is missing the sender key")]
    MissingSenderKey,

    /// The sender of the plaintext doesn't match the sender of the encrypted
    /// message.
    #[error(
        "the sender of the plaintext doesn't match the sender of the encrypted \
        message, got {0}, expected {1}"
    )]
    MismatchedSender(OwnedUserId, OwnedUserId),

    /// The public key that was part of the message doesn't match the key we
    /// have stored.
    #[error(
        "the public key that was part of the message doesn't match the key we \
        have stored, expected {0}, got {1}"
    )]
    MismatchedKeys(Box<Ed25519PublicKey>, Box<Ed25519PublicKey>),

    /// The room ID of the room key doesn't match the room ID of the decrypted
    /// event.
    #[error(
        "the room id of the room key doesn't match the room id of the \
        decrypted event: expected {0}, got {1:?}"
    )]
    MismatchedRoom(OwnedRoomId, Option<OwnedRoomId>),

    /// The event includes `sender_device_keys` as per [MSC4147], but the
    /// signature was invalid, or the ed25519 or curve25519 key did not
    /// match other data in the event.
    ///
    /// [MSC4147]: https://github.com/matrix-org/matrix-spec-proposals/pull/4147
    #[error("the event included sender_device_keys which were invalid in some way")]
    InvalidSenderDeviceKeys,
}

/// Error type describing different errors that can happen when we create an
/// Olm session from a pickle.
#[derive(Error, Debug)]
pub enum SessionUnpickleError {
    /// The device keys are missing the signing key
    #[error("the device keys are missing the signing key")]
    MissingSigningKey,

    /// The device keys are missing the identity key
    #[error("the device keys are missing the identity key")]
    MissingIdentityKey,
}

/// Error type describing different errors that happen when we check or create
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

    /// The signature could not be decoded.
    #[error("the given signature is not valid and can't be decoded")]
    InvalidSignature,

    /// The signing key that used to sign the object has been changed.
    #[error("the signing key that used to sign the object has changed, old: {0:?}, new: {1:?}")]
    SigningKeyChanged(Option<Box<Ed25519PublicKey>>, Option<Box<Ed25519PublicKey>>),

    /// The signed object couldn't be deserialized.
    #[error(transparent)]
    JsonError(#[from] CanonicalJsonError),

    /// The store ran into an error.
    #[error(transparent)]
    StoreError(#[from] CryptoStoreError),
}

/// Error that occurs when a room key can't be converted into a valid Megolm
/// session.
#[derive(Error, Debug)]
pub enum SessionCreationError {
    /// The requested one-time key isn't a signed curve key.
    #[error(
        "Failed to create a new Olm session for {0} {1}, the requested \
        one-time key isn't a signed curve key"
    )]
    OneTimeKeyNotSigned(OwnedUserId, OwnedDeviceId),

    /// The signed one-time key is missing.
    #[error(
        "Tried to create a new Olm session for {0} {1}, but the signed \
        one-time key is missing"
    )]
    OneTimeKeyMissing(OwnedUserId, OwnedDeviceId),

    /// Failed to verify the one-time key signatures.
    #[error(
        "Failed to verify the signature of a one-time key, key: {one_time_key:?}, \
        signing_key: {signing_key:?}: {error:?}"
    )]
    InvalidSignature {
        /// The one-time key that failed the signature verification.
        one_time_key: Box<SignedKey>,
        /// The key that was used to verify the signature.
        signing_key: Option<Box<Ed25519PublicKey>>,
        /// The exact error describing why the signature verification failed.
        error: Box<SignatureError>,
    },

    /// The user's device is missing a curve25519 key.
    #[error(
        "Tried to create an Olm session for {0} {1}, but the device is missing \
        a curve25519 key"
    )]
    DeviceMissingCurveKey(OwnedUserId, OwnedDeviceId),

    /// Error deserializing the one-time key.
    #[error("Error deserializing the one-time key: {0}")]
    InvalidJson(#[from] serde_json::Error),

    /// The given curve25519 key is not a valid key.
    #[error("The given curve25519 key is not a valid key")]
    InvalidCurveKey(#[from] vodozemac::KeyError),

    /// Error when creating an Olm Session from an incoming Olm message.
    #[error(transparent)]
    InboundCreation(#[from] vodozemac::olm::SessionCreationError),

    /// The given device keys are invalid.
    #[error("The given device keys are invalid")]
    InvalidDeviceKeys(#[from] SignatureError),

    /// There was a mismatch between the identity keys of the device we received
    /// the room key from and the identity keys recorded in the plaintext of the
    /// room key to-device message.
    #[error(
        "There was a mismatch between the identity keys of the sending device \
        and those recorded in the to-device message"
    )]
    MismatchedIdentityKeys(MismatchedIdentityKeysError),
}

/// Errors that can be returned by
/// [`crate::machine::OlmMachine::set_room_settings`].
#[derive(Debug, Error)]
pub enum SetRoomSettingsError {
    /// The changes are rejected because they conflict with the previous
    /// settings for this room.
    #[error("the new settings would cause a downgrade of encryption security")]
    EncryptionDowngrade,

    /// The changes are rejected because we would be unable to use them to
    /// encrypt events.
    #[error("the new settings are invalid")]
    InvalidSettings,

    /// The store ran into an error.
    #[error(transparent)]
    Store(#[from] CryptoStoreError),
}

/// Error representing a problem when collecting the recipient devices for the
/// room key, during an encryption operation.
#[derive(Error, Debug)]
pub enum SessionRecipientCollectionError {
    /// One or more verified users has one or more unsigned devices.
    ///
    /// Happens only with [`CollectStrategy::ErrorOnVerifiedUserProblem`].
    ///
    /// In order to resolve this, the caller can set the trust level of the
    /// affected devices to [`LocalTrust::Ignored`] or
    /// [`LocalTrust::BlackListed`] (see [`Device::set_local_trust`]), and
    /// then retry the encryption operation.
    #[error("one or more verified users have unsigned devices")]
    VerifiedUserHasUnsignedDevice(BTreeMap<OwnedUserId, Vec<OwnedDeviceId>>),

    /// One or more users was previously verified, but they have changed their
    /// identity.
    ///
    /// Happens only with [`CollectStrategy::ErrorOnVerifiedUserProblem`] or
    /// [`CollectStrategy::IdentityBasedStrategy`].
    ///
    /// In order to resolve this, the user can:
    ///
    /// * re-verify the problematic recipients, or
    ///
    /// * withdraw verification of the problematic recipients with
    ///   [`OtherUserIdentity::withdraw_verification`], or
    ///
    /// * set the trust level of all of the devices belonging to the problematic
    ///   recipients to [`LocalTrust::Ignored`] or [`LocalTrust::BlackListed`]
    ///   (see [`Device::set_local_trust`]).
    ///
    /// The caller can then retry the encryption operation.
    #[error("one or more users that were verified have changed their identity")]
    VerifiedUserChangedIdentity(Vec<OwnedUserId>),

    /// Cross-signing has not been configured on our own identity.
    ///
    /// Happens only with [`CollectStrategy::IdentityBasedStrategy`].
    /// (Cross-signing is required for encryption when using
    /// `IdentityBasedStrategy`.) Apps should detect this condition and prevent
    /// sending in the UI rather than waiting for this error to be returned when
    /// encrypting.
    #[error("Encryption failed because cross-signing is not set up on your account")]
    CrossSigningNotSetup,

    /// The current device has not been cross-signed by our own identity.
    ///
    /// Happens only with [`CollectStrategy::IdentityBasedStrategy`].
    /// (Cross-signing is required for encryption when using
    /// `IdentityBasedStrategy`.) Apps should detect this condition and prevent
    /// sending in the UI rather than waiting for this error to be returned when
    /// encrypting.
    #[error("Encryption failed because your device is not verified")]
    SendingFromUnverifiedDevice,
}
