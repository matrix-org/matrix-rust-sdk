// Copyright 2023 The Matrix.org Foundation C.I.C.
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

//! Helpers for implementing the Secrets Storage mechanism from the Matrix
//! [spec].
//!
//! [spec]: https://spec.matrix.org/v1.8/client-server-api/#storage

use std::fmt;

use hmac::{digest::MacError, Hmac};
use pbkdf2::pbkdf2;
use rand::{
    distributions::{Alphanumeric, DistString},
    thread_rng, RngCore,
};
use ruma::{
    events::{
        secret::request::SecretName,
        secret_storage::{
            key::{
                PassPhrase, SecretStorageEncryptionAlgorithm, SecretStorageKeyEventContent,
                SecretStorageV1AesHmacSha2Properties,
            },
            secret::SecretEncryptedData,
        },
    },
    serde::Base64,
    UInt,
};
use serde::de::Error;
use sha2::Sha512;
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::ciphers::{AesHmacSha2Key, HmacSha256Mac, IV_SIZE, KEY_SIZE, MAC_SIZE};

/// Error type for the decoding of a [`SecretStorageKey`].
#[derive(Debug, Error)]
pub enum DecodeError {
    /// The decoded secret storage key has an invalid prefix.
    #[error("The decoded secret storage key has an invalid prefix: expected {0:?}, got {1:?}")]
    Prefix([u8; 2], [u8; 2]),
    /// The parity byte of the secret storage key didn't match.
    #[error("The parity byte of the secret storage key doesn't match: expected {0:?}, got {1:?}")]
    Parity(u8, u8),
    /// The secret storage key isn't valid Base58.
    #[error(transparent)]
    Base58(#[from] bs58::decode::Error),
    /// The secret storage key isn't valid Base64.
    #[error(transparent)]
    Base64(#[from] vodozemac::Base64DecodeError),
    /// The secret storage key is too short, we couldn't read enough data.
    #[error("The Base58 decoded key has an invalid length, expected {0}, got {1}")]
    KeyLength(usize, usize),
    /// The typed in secret storage was incorrect, the MAC check failed.
    #[error("The MAC check for the secret storage key failed")]
    Mac(#[from] MacError),
    /// The MAC of the secret storage key for the MAC check has an incorrect
    /// length.
    #[error("The MAC of for the secret storage MAC check has an incorrect length, expected: {0}, got: {1}")]
    MacLength(usize, usize),
    /// The IV of the secret storage key for the MAC check has an incorrect
    /// length.
    #[error("The IV of for the secret storage key MAC check has an incorrect length, expected: {0}, got: {1}")]
    IvLength(usize, usize),
    /// The secret storage key is using an unsupported secret encryption
    /// algorithm. Currently only the [`m.secret_storage.v1.aes-hmac-sha2`]
    /// algorithm is supported.
    ///
    /// [`m.secret_storage.v1.aes-hmac-sha2`]: https://spec.matrix.org/v1.8/client-server-api/#msecret_storagev1aes-hmac-sha2
    #[error("The secret storage key is using an unsupported secret encryption algorithm: {0}")]
    UnsupportedAlgorithm(String),
    /// The passphrase-based secret storage key has an excessively high KDF
    /// iteration count.
    #[error(
        "The passphrase-based secret storage key has an excessively high KDF iteration count: {0}"
    )]
    KdfIterationCount(UInt),
}

/// A secret storage key which can be used to store encrypted data in the user's
/// account data as defined in the [spec].
///
/// The secret storage key can be initialized from a passphrase or from a
/// base58-encoded string.
///
/// To bootstrap a new [`SecretStorageKey`], use the [`SecretStorageKey::new()`]
/// or [`SecretStorageKey::new_from_passphrase()`] method.
///
/// After a new [`SecretStorageKey`] has been created, the info about the key
/// needs to be uploaded to the homeserver as a global account data event. The
/// event and event type for this can be retrieved using the
/// [`SecretStorageKey::event_content()`] and [`SecretStorageKey::event_type()`]
/// methods, respectively.
///
/// # Examples
/// ```no_run
/// use matrix_sdk_crypto::secret_storage::SecretStorageKey;
///
/// // Create a new secret storage key.
/// let key =
///     SecretStorageKey::new_from_passphrase("It's a secret to everybody");
/// // Retrieve the content.
/// let content = key.event_content();
/// // Now upload the content to the server and mark the new key as the default one.
///
/// // If we want to restore the secret key, we'll need to retrieve the previously uploaded global
/// // account data event.
/// let restored_key = SecretStorageKey::from_account_data(
///     "It's a secret to everybody",
///     content.to_owned()
/// );
/// ```
///
/// [spec]: https://spec.matrix.org/v1.8/client-server-api/#secret-storage
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretStorageKey {
    /// Information about the secret storage key.
    ///
    /// This is uploaded to the homeserver in a global account data event.
    #[zeroize(skip)]
    storage_key_info: SecretStorageKeyEventContent,
    /// The private key material.
    secret_key: Box<[u8; 32]>,
}

impl fmt::Debug for SecretStorageKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretStorageKey")
            .field("storage_key_info", &self.storage_key_info)
            .finish_non_exhaustive()
    }
}

/// Encrypted data for the AES-CTR/HMAC-SHA-256 secret storage algorithm.
#[derive(Debug)]
pub struct AesHmacSha2EncryptedData {
    /// The initialization vector that was used to encrypt the ciphertext.
    pub iv: [u8; IV_SIZE],
    /// The ciphertext of the message.
    pub ciphertext: Base64,
    /// The message authentication code ensuring that the message was not
    /// forged.
    pub mac: [u8; MAC_SIZE],
}

impl TryFrom<SecretEncryptedData> for AesHmacSha2EncryptedData {
    type Error = serde_json::Error;

    fn try_from(value: SecretEncryptedData) -> Result<Self, Self::Error> {
        match value {
            SecretEncryptedData::AesHmacSha2EncryptedData { iv, ciphertext, mac } => {
                let iv_length = iv.as_bytes().len();
                let mac_length = mac.as_bytes().len();

                if iv_length != IV_SIZE {
                    Err(serde_json::Error::custom(format!(
                        "Invalid initialization vector length, expected length {IV_SIZE}, got: {iv_length}",
                    )))
                } else if mac_length != MAC_SIZE {
                    Err(serde_json::Error::custom(format!(
                        "Invalid message authentication tag length, expected length {MAC_SIZE}, got: {mac_length}",
                    )))
                } else {
                    let mut mac_array = [0u8; MAC_SIZE];
                    let mut iv_array = [0u8; IV_SIZE];

                    mac_array.copy_from_slice(mac.as_bytes());
                    iv_array.copy_from_slice(iv.as_bytes());

                    Ok(Self { iv: iv_array, ciphertext, mac: mac_array })
                }
            }
            _ => Err(serde_json::Error::custom("Unsupported secret storage algorithm")),
        }
    }
}

impl From<AesHmacSha2EncryptedData> for SecretEncryptedData {
    fn from(value: AesHmacSha2EncryptedData) -> Self {
        SecretEncryptedData::AesHmacSha2EncryptedData {
            iv: Base64::new(value.iv.to_vec()),
            ciphertext: value.ciphertext,
            mac: Base64::new(value.mac.to_vec()),
        }
    }
}

impl SecretStorageKey {
    const ZERO_MESSAGE: &[u8; 32] = &[0u8; 32];
    const PREFIX: [u8; 2] = [0x8b, 0x01];
    const PREFIX_PARITY: u8 = Self::PREFIX[0] ^ Self::PREFIX[1];
    const DEFAULT_KEY_ID_LEN: usize = 32;
    #[cfg(not(test))]
    const DEFAULT_PBKDF_ITERATIONS: u32 = 500_000;
    #[cfg(test)]
    const DEFAULT_PBKDF_ITERATIONS: u32 = 10;

    // 35 bytes in total: a 2-byte prefix, 32 bytes for the key material and one
    // parity byte
    const DECODED_BASE58_KEY_LEN: usize = 2 + 32 + 1;

    /// Calculate a parity byte for the base58-encoded variant of the
    /// [`SecretStorageKey`]. Described in the [spec].
    ///
    /// [spec]: https://spec.matrix.org/v1.8/client-server-api/#key-representation
    fn parity_byte(bytes: &[u8]) -> u8 {
        bytes.iter().fold(Self::PREFIX_PARITY, |acc, x| acc ^ x)
    }

    /// Check that the [`SecretStorageKey`] is the one described in the given
    /// [`SecretEncryptionAlgorithm`].
    ///
    /// This is done by encrypting a message containing zero bytes and comparing
    /// the MAC of this encrypted message to the MAC given in the
    /// [`SecretEncryptionAlgorithm`]. The exact steps are described in the
    /// [spec].
    ///
    /// This check needs to be done every time we restore a [`SecretStorageKey`]
    /// from a passphrase or from the base58-encoded variant of it.
    ///
    /// [spec]: https://spec.matrix.org/v1.8/client-server-api/#msecret_storagev1aes-hmac-sha2
    fn check_zero_message(&self) -> Result<(), DecodeError> {
        match &self.storage_key_info.algorithm {
            SecretStorageEncryptionAlgorithm::V1AesHmacSha2(properties) => {
                if properties.iv.as_bytes().len() != IV_SIZE {
                    Err(DecodeError::IvLength(IV_SIZE, properties.iv.as_bytes().len()))
                } else {
                    let mut iv_array = [0u8; 16];
                    iv_array.copy_from_slice(properties.iv.as_bytes());

                    // I'm not particularly convinced that this couldn't have been done simpler. Why
                    // do we need to reproduce the ciphertext? Couldn't we just generate the MAC tag
                    // using the `ZERO_MESSAGE`?
                    //
                    // If someone is reading this and is designing a new secret encryption
                    // algorithm, please consider the above suggestion.
                    let key = AesHmacSha2Key::from_secret_storage_key(&self.secret_key, "");
                    let ciphertext = key.apply_keystream(Self::ZERO_MESSAGE.to_vec(), &iv_array);
                    let expected_mac = HmacSha256Mac::from_slice(properties.mac.as_bytes())
                        .ok_or_else(|| {
                            DecodeError::MacLength(MAC_SIZE, properties.mac.as_bytes().len())
                        })?;

                    key.verify_mac(&ciphertext, expected_mac.as_bytes())?;

                    Ok(())
                }
            }
            custom => Err(DecodeError::UnsupportedAlgorithm(custom.algorithm().to_owned())),
        }
    }

    fn create_event_content(key_id: String, key: &[u8; KEY_SIZE]) -> SecretStorageKeyEventContent {
        let key = AesHmacSha2Key::from_secret_storage_key(key, "");

        let (ciphertext, iv) = key.encrypt(Self::ZERO_MESSAGE.to_vec());
        let iv = Base64::new(iv.to_vec());
        let mac = Base64::new(key.create_mac_tag(&ciphertext).as_bytes().to_vec());

        SecretStorageKeyEventContent::new(
            key_id,
            SecretStorageEncryptionAlgorithm::V1AesHmacSha2(
                SecretStorageV1AesHmacSha2Properties::new(iv, mac),
            ),
        )
    }

    /// Create a new random [`SecretStorageKey`].
    pub fn new() -> Self {
        let mut key = Box::new([0u8; KEY_SIZE]);
        let mut rng = thread_rng();
        rng.fill_bytes(key.as_mut_slice());

        let key_id = Alphanumeric.sample_string(&mut rng, Self::DEFAULT_KEY_ID_LEN);

        Self::from_bytes(key_id, key)
    }

    /// Create a new passphrase-based [`SecretStorageKey`].
    ///
    /// The passphrase will be expanded into a 32-byte key using the `m.pbkdf2`
    /// algorithm described in the [spec].
    ///
    /// [spec]: https://spec.matrix.org/v1.8/client-server-api/#deriving-keys-from-passphrases
    pub fn new_from_passphrase(passphrase: &str) -> Self {
        let mut key = Box::new([0u8; 32]);
        let mut rng = thread_rng();
        let salt = Alphanumeric.sample_string(&mut rng, Self::DEFAULT_KEY_ID_LEN);

        pbkdf2::<Hmac<Sha512>>(
            passphrase.as_bytes(),
            salt.as_bytes(),
            Self::DEFAULT_PBKDF_ITERATIONS,
            key.as_mut_slice(),
        )
        .expect(
            "We should be able to expand a passphrase of any length due to \
             HMAC being able to be initialized with any input size",
        );

        let key_id = Alphanumeric.sample_string(&mut rng, Self::DEFAULT_KEY_ID_LEN);
        let mut key = Self::from_bytes(key_id, key);

        key.storage_key_info.passphrase =
            Some(PassPhrase::new(salt, Self::DEFAULT_PBKDF_ITERATIONS.into()));

        key
    }

    pub(crate) fn from_bytes(key_id: String, key: Box<[u8; KEY_SIZE]>) -> Self {
        let storage_key_info = Self::create_event_content(key_id.to_owned(), &key);

        Self { storage_key_info, secret_key: key }
    }

    /// Restore a [`SecretStorageKey`] from the given input and the description
    /// of the key.
    ///
    /// The [`SecretStorageKeyEventContent`] will contain the description of the
    /// [`SecretStorageKey`]. The constructor will check if the provided input
    /// string matches to the description.
    ///
    /// The input can be a passphrase or a Base58 export of the
    /// [`SecretStorageKey`].
    pub fn from_account_data(
        input: &str,
        content: SecretStorageKeyEventContent,
    ) -> Result<Self, DecodeError> {
        let key = if let Some(passphrase_info) = &content.passphrase {
            // If the content defines a passphrase, first try treating the input
            // as a passphrase.
            match Self::from_passphrase(input, &content, passphrase_info) {
                Ok(key) => key,
                // Let us fallback to Base58 now. If that fails as well, return the original,
                // passphrase-based error.
                Err(e) => Self::from_base58(input, &content).map_err(|_| e)?,
            }
        } else {
            // No passphrase info, so it must be base58-encoded.
            Self::from_base58(input, &content)?
        };

        Ok(key)
    }

    fn from_passphrase(
        passphrase: &str,
        key_info: &SecretStorageKeyEventContent,
        passphrase_info: &PassPhrase,
    ) -> Result<Self, DecodeError> {
        let mut key = Box::new([0u8; 32]);
        pbkdf2::<Hmac<Sha512>>(
            passphrase.as_bytes(),
            passphrase_info.salt.as_bytes(),
            passphrase_info
                .iterations
                .try_into()
                .map_err(|_| DecodeError::KdfIterationCount(passphrase_info.iterations))?,
            key.as_mut_slice(),
        )
        .expect(
            "We should be able to expand a passphrase of any length due to \
             HMAC being able to be initialized with any input size",
        );

        let key = Self { storage_key_info: key_info.to_owned(), secret_key: key };
        key.check_zero_message()?;

        Ok(key)
    }

    // Parse a secret storage key represented as a base58-encoded string.
    //
    // This method reverses the process in the [`SecretStorageKey::to_base58()`]
    // method.
    fn parse_base58_key(value: &str) -> Result<Box<[u8; 32]>, DecodeError> {
        // The spec tells us to remove any whitespace:
        // > When decoding a raw key, the process should be reversed, with the exception
        // > that whitespace is insignificant in the user’s input.
        //
        // Spec link: https://spec.matrix.org/unstable/client-server-api/#key-representation
        let value: String = value.chars().filter(|c| !c.is_whitespace()).collect();

        let mut decoded = bs58::decode(value).with_alphabet(bs58::Alphabet::BITCOIN).into_vec()?;

        let mut prefix = [0u8; 2];
        let mut key = Box::new([0u8; 32]);

        let decoded_len = decoded.len();

        if decoded_len != Self::DECODED_BASE58_KEY_LEN {
            Err(DecodeError::KeyLength(Self::DECODED_BASE58_KEY_LEN, decoded_len))
        } else {
            prefix.copy_from_slice(&decoded[0..2]);
            key.copy_from_slice(&decoded[2..34]);
            let expected_parity = decoded[34];

            decoded.zeroize();

            let parity = Self::parity_byte(key.as_ref());

            let unexpected_choice = prefix.ct_ne(&Self::PREFIX);
            let unexpected_parity = expected_parity.ct_ne(&parity);

            if unexpected_choice.into() {
                Err(DecodeError::Prefix(Self::PREFIX, prefix))
            } else if unexpected_parity.into() {
                Err(DecodeError::Parity(expected_parity, parity))
            } else {
                Ok(key)
            }
        }
    }

    /// Try to create a [`SecretStorageKey`] from a Base58 export.
    fn from_base58(
        value: &str,
        key_info: &SecretStorageKeyEventContent,
    ) -> Result<Self, DecodeError> {
        let secret_key = Self::parse_base58_key(value)?;
        let key = Self { storage_key_info: key_info.to_owned(), secret_key };
        key.check_zero_message()?;

        Ok(key)
    }

    /// Export the [`SecretStorageKey`] as a base58-encoded string as defined in
    /// the [spec].
    ///
    /// *Note*: This returns a copy of the private key material of the
    /// [`SecretStorageKey`] as a string. The caller needs to ensure that this
    /// string is zeroized.
    ///
    /// [spec]: https://spec.matrix.org/v1.8/client-server-api/#key-representation
    pub fn to_base58(&self) -> String {
        const DISPLAY_CHUNK_SIZE: usize = 4;

        let mut bytes = Box::new([0u8; Self::DECODED_BASE58_KEY_LEN]);

        // The key is prepended by the two prefix bytes, 0x8b and 0x01.
        bytes[0..2].copy_from_slice(Self::PREFIX.as_slice());
        bytes[2..34].copy_from_slice(self.secret_key.as_slice());

        // All the bytes in the string above, including the two header bytes, are XORed
        // together to form a parity byte. This parity byte is appended to the byte
        // string.
        bytes[34] = Self::parity_byte(self.secret_key.as_slice());

        // The byte string is encoded using Base58, using the same mapping as is used
        // for Bitcoin addresses.
        let base_58 =
            bs58::encode(bytes.as_slice()).with_alphabet(bs58::Alphabet::BITCOIN).into_string();

        bytes.zeroize();

        // The string is formatted into groups of four characters separated by spaces.
        let ret = base_58
            .chars()
            .collect::<Vec<char>>()
            .chunks(DISPLAY_CHUNK_SIZE)
            .map(|c| c.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join(" ");

        ret
    }

    /// Encrypt a given secret string as a Secrets Storage secret with the
    /// given secret name.
    ///
    /// # Examples
    ///
    /// ```
    /// use matrix_sdk_crypto::secret_storage::SecretStorageKey;
    /// use ruma::events::secret::request::SecretName;
    ///
    /// let key = SecretStorageKey::new();
    /// let secret = "It's a secret to everybody";
    /// let secret_name = SecretName::from("my-secret");
    ///
    /// let encrypted_data = key.encrypt(secret.as_bytes().to_vec(), &secret_name);
    ///
    /// let decrypted = key.decrypt(&encrypted_data, &secret_name)?;
    ///
    /// assert_eq!(secret.as_bytes(), decrypted);
    /// # anyhow::Ok(())
    /// ```
    pub fn encrypt(
        &self,
        plaintext: Vec<u8>,
        secret_name: &SecretName,
    ) -> AesHmacSha2EncryptedData {
        let key = AesHmacSha2Key::from_secret_storage_key(&self.secret_key, secret_name.as_str());

        let (ciphertext, iv) = key.encrypt(plaintext);
        let mac = key.create_mac_tag(&ciphertext).into_bytes();
        let ciphertext = Base64::new(ciphertext);

        AesHmacSha2EncryptedData { iv, ciphertext, mac }
    }

    /// Decrypt the given [`AesHmacSha2EncryptedData`] containing a secret with
    /// the given secret name.
    pub fn decrypt(
        &self,
        data: &AesHmacSha2EncryptedData,
        secret_name: &SecretName,
    ) -> Result<Vec<u8>, MacError> {
        let key = AesHmacSha2Key::from_secret_storage_key(&self.secret_key, secret_name.as_str());
        let ciphertext = data.ciphertext.to_owned().into_inner();

        key.verify_mac(&ciphertext, &data.mac)?;

        let plaintext = key.decrypt(ciphertext, &data.iv);

        Ok(plaintext)
    }

    /// The info about the [`SecretStorageKey`] formatted as a
    /// [`SecretStorageKeyEventContent`].
    ///
    /// The [`SecretStorageKeyEventContent`] contains information about the
    /// secret storage key. This information can be used to determine whether
    /// the secret the user has entered is a valid secret for unlocking the
    /// Secrets Storage (i.e. a valid [`SecretStorageKey`]).
    pub fn event_content(&self) -> &SecretStorageKeyEventContent {
        &self.storage_key_info
    }

    /// The unique ID of this [`SecretStorageKey`].
    pub fn key_id(&self) -> &str {
        &self.storage_key_info.key_id
    }

    /// The event type of this [`SecretStorageKey`].
    ///
    /// Can be used when uploading the key info as a
    /// [`SecretStorageKeyEventContent`] to the homeserver.
    ///
    /// The type is equal to the concatenation of the string
    /// `"m.secret_storage.key."` and the key ID from the
    /// [`SecretStorageKey::key_id()`] method.
    pub fn event_type(&self) -> String {
        format!("m.secret_storage.key.{}", self.key_id())
    }
}

#[cfg(test)]
mod test {
    use assert_matches::assert_matches;
    use ruma::events::EventContentFromType;
    use serde_json::{json, value::to_raw_value};

    use super::*;

    const SECRET_STORAGE_KEY: &[u8; 32] = &[0u8; 32];

    #[test]
    fn encrypting() {
        let secret = "It's a secret to everybody";
        let secret_name = SecretName::from("secret_message");

        let key = SecretStorageKey::from_bytes(
            "key_id".to_owned(),
            Box::new(SECRET_STORAGE_KEY.to_owned()),
        );

        let encrypted = key.encrypt(secret.as_bytes().to_vec(), &secret_name);
        let decrypted = key
            .decrypt(&encrypted, &secret_name)
            .expect("We should be able to decrypt the message we just encrypted");

        assert_eq!(
            secret.as_bytes(),
            decrypted,
            "Encryption roundtrip should result in the same plaintext"
        );
    }

    #[test]
    fn from_passphrase_roundtrip() {
        let passphrase = "It's a secret to everybody";
        let secret = "Foobar";
        let secret_name = SecretName::from("secret_message");

        let key = SecretStorageKey::new_from_passphrase("It's a secret to everybody");

        let encrypted = key.encrypt(secret.as_bytes().to_vec(), &secret_name);
        let content = to_raw_value(key.event_content())
            .expect("We should be able to serialize the secret storage key event content");

        let content = SecretStorageKeyEventContent::from_parts(&key.event_type(), &content).expect(
            "We should be able to parse our, just serialized, secret storage key event content",
        );

        let key = SecretStorageKey::from_account_data(passphrase, content)
            .expect("We should be able to restore our secret storage key");

        let decrypted = key.decrypt(&encrypted, &secret_name).expect(
            "We should be able to decrypt the message using the restored secret storage key",
        );

        assert_eq!(
            secret.as_bytes(),
            decrypted,
            "The encryption roundtrip should produce the same plaintext"
        );
    }

    #[test]
    fn from_base58_roundtrip() {
        let secret = "Foobar";
        let secret_name = SecretName::from("secret_message");

        let key = SecretStorageKey::new();

        let encrypted = key.encrypt(secret.as_bytes().to_vec(), &secret_name);
        let content = to_raw_value(key.event_content())
            .expect("We should be able to serialize the secret storage key event content");

        let content = SecretStorageKeyEventContent::from_parts(&key.event_type(), &content).expect(
            "We should be able to parse our, just serialized, secret storage key event content",
        );

        let base58_key = key.to_base58();

        let key = SecretStorageKey::from_account_data(&base58_key, content)
            .expect("We should be able to restore our secret storage key");

        let decrypted = key.decrypt(&encrypted, &secret_name).expect(
            "We should be able to decrypt the message using the restored secret storage key",
        );

        assert_eq!(
            secret.as_bytes(),
            decrypted,
            "The encryption roundtrip should produce the same plaintext"
        );
    }

    #[test]
    fn from_account_data_and_passphrase() {
        let json = to_raw_value(&json!({
            "algorithm":"m.secret_storage.v1.aes-hmac-sha2",
            "iv":"gH2iNpiETFhApvW6/FFEJQ",
            "mac":"9Lw12m5SKDipNghdQXKjgpfdj1/K7HFI2brO+UWAGoM",
            "passphrase":{
                "algorithm":"m.pbkdf2",
                "salt":"IuLnH7S85YtZmkkBJKwNUKxWF42g9O1H",
                "iterations":10
            }
        }))
        .unwrap();

        let content = SecretStorageKeyEventContent::from_parts(
            "m.secret_storage.key.DZkbKc0RtKSq0z8V61w6KBmJCK6OCiIu",
            &json,
        )
        .expect("We should be able to deserialize our static secret storage key");

        SecretStorageKey::from_account_data("It's a secret to everybody", content)
            .expect("We should be able to restore the secret storage key");
    }

    #[test]
    fn from_account_data_and_base58() {
        let base58_key = "EsTj 3yST y93F SLpB jJsz eAXc 2XzA ygD3 w69H fGaN TKBj jXEd";

        let json = to_raw_value(&json!({
            "algorithm": "m.secret_storage.v1.aes-hmac-sha2",
            "iv": "xv5b6/p3ExEw++wTyfSHEg==",
            "mac": "ujBBbXahnTAMkmPUX2/0+VTfUh63pGyVRuBcDMgmJC8="
        }))
        .unwrap();

        let content = SecretStorageKeyEventContent::from_parts(
            "m.secret_storage.key.bmur2d9ypPUH1msSwCxQOJkuKRmJI55e",
            &json,
        )
        .expect("We should be able to deserialize our static secret storage key");

        SecretStorageKey::from_account_data(base58_key, content)
            .expect("We should be able to restore the secret storage key");
    }

    #[test]
    fn invalid_key() {
        let key = SecretStorageKey::new_from_passphrase("It's a secret to everybody");

        let content = to_raw_value(key.event_content())
            .expect("We should be able to serialize the secret storage key event content");

        let content = SecretStorageKeyEventContent::from_parts(&key.event_type(), &content).expect(
            "We should be able to parse our, just serialized, secret storage key event content",
        );

        assert_matches!(
            SecretStorageKey::from_account_data("It's a secret to nobody", content.to_owned()),
            Err(DecodeError::Mac(_)),
            "Using the wrong passphrase should throw a MAC error"
        );

        let key = SecretStorageKey::new();
        let base58_key = key.to_base58();

        assert_matches!(
            SecretStorageKey::from_account_data(&base58_key, content),
            Err(DecodeError::Mac(_)),
            "Using the wrong base58 key should throw a MAC error"
        );
    }

    #[test]
    fn base58_parsing() {
        const DECODED_KEY: [u8; 32] = [
            159, 189, 70, 187, 52, 81, 113, 198, 246, 2, 44, 154, 37, 213, 104, 27, 165, 78, 236,
            106, 108, 73, 83, 243, 173, 192, 185, 110, 157, 145, 173, 163,
        ];

        let key = "EsT           pRvZTnjck8    YrhRAtw XLS84Nr2r9S9LGAWDaExVAPBvLRK   ";
        let parsed_key = SecretStorageKey::parse_base58_key(key)
            .expect("Whitespace in the Base58 encoded key should not matter");

        assert_eq!(
            parsed_key.as_slice(),
            DECODED_KEY,
            "Decoding the key should produce the correct bytes"
        );

        let key = "EsTpRvZTnjck8YrhRAtwXLS84Nr2r9S9LGAWDaExVAPBvLRk";
        assert_matches!(
            SecretStorageKey::parse_base58_key(key),
            Err(DecodeError::Parity(..)),
            "We should detect an invalid parity byte"
        );

        let key = "AATpRvZTnjck8YrhRAtwXLS84Nr2r9S9LGAWDaExVAPBvLRk";
        assert_matches!(
            SecretStorageKey::parse_base58_key(key),
            Err(DecodeError::Prefix(..)),
            "We should detect an invalid prefix"
        );

        let key = "AATpRvZTnjck8YrhRAtwXLS84Nr2r9S9";
        assert_matches!(
            SecretStorageKey::parse_base58_key(key),
            Err(DecodeError::KeyLength(..)),
            "We should detect if the key isn't of the correct length"
        );

        let key = "AATpRvZTnjck8YrhRAtwXLS84Nr0OIl";
        assert_matches!(
            SecretStorageKey::parse_base58_key(key),
            Err(DecodeError::Base58(..)),
            "We should detect if the key isn't Base58"
        );
    }

    #[test]
    fn encrypted_data_decoding() {
        let json = json!({
              "iv": "bdfCwu+ECYgZ/jWTkGrQ/A==",
              "ciphertext": "lCRSSA1lChONEXj/8RyogsgAa8ouQwYDnLr4XBCheRikrZykLRzPCx3doCE=",
              "mac": "NXeV1dZaOe2JLvQ6Hh6tFto7AgFFdaQnY0l9pruwdtE="
        });

        let content: SecretEncryptedData = serde_json::from_value(json)
            .expect("We should be able to deserialize our static JSON content");

        let encrypted_data: AesHmacSha2EncryptedData = content.try_into()
            .expect("We should be able to convert a valid SecretEncryptedData to a AesHmacSha2EncryptedData struct");

        assert_eq!(
            encrypted_data.mac,
            [
                53, 119, 149, 213, 214, 90, 57, 237, 137, 46, 244, 58, 30, 30, 173, 22, 218, 59, 2,
                1, 69, 117, 164, 39, 99, 73, 125, 166, 187, 176, 118, 209
            ]
        );
        assert_eq!(
            encrypted_data.iv,
            [109, 215, 194, 194, 239, 132, 9, 136, 25, 254, 53, 147, 144, 106, 208, 252]
        );

        let invalid_mac_json = json!({
              "iv": "bdfCwu+ECYgZ/jWTkGrQ/A==",
              "ciphertext": "lCRSSA1lChONEXj/8RyogsgAa8ouQwYDnLr4XBCheRikrZykLRzPCx3doCE=",
              "mac": "NXeV1dZaOe2JLvQ6Hh6tFtgFFdaQnY0l9pruwdtE"
        });

        let content: SecretEncryptedData = serde_json::from_value(invalid_mac_json)
            .expect("We should be able to deserialize our static JSON content");

        let encrypted_data: Result<AesHmacSha2EncryptedData, _> = content.try_into();
        encrypted_data.expect_err(
            "We should be able to detect if a SecretEncryptedData content has an invalid MAC",
        );

        let invalid_iv_json = json!({
              "iv": "bdfCwu+gZ/jWTkGrQ/A",
              "ciphertext": "lCRSSA1lChONEXj/8RyogsgAa8ouQwYDnLr4XBCheRikrZykLRzPCx3doCE=",
              "mac": "NXeV1dZaOe2JLvQ6Hh6tFto7AgFFdaQnY0l9pruwdtE="
        });

        let content: SecretEncryptedData = serde_json::from_value(invalid_iv_json)
            .expect("We should be able to deserialize our static JSON content");

        let encrypted_data: Result<AesHmacSha2EncryptedData, _> = content.try_into();
        encrypted_data.expect_err(
            "We should be able to detect if a SecretEncryptedData content has an invalid IV",
        );
    }
}
