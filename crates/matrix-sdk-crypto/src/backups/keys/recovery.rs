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

use std::{
    convert::TryFrom,
    io::{Cursor, Read},
};

use aes::cipher::generic_array::GenericArray;
use aes_gcm::{
    aead::{Aead, NewAead},
    Aes256Gcm,
};
use bs58;
use olm_rs::{
    errors::OlmPkDecryptionError,
    pk::{OlmPkDecryption, PkMessage},
};
use rand::{thread_rng, Error as RandomError, Fill};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

use super::MegolmV1BackupKey;
use crate::utilities::{decode_url_safe, encode, encode_url_safe};

const NONCE_SIZE: usize = 12;

/// Error type for the decoding of a RecoveryKey.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// The decoded recovery key has an invalid prefix.
    #[error("The decoded recovery key has an invalid prefix: expected {0:?}, got {1:?}")]
    Prefix([u8; 2], [u8; 2]),
    /// The parity byte of the recovery key didn't match.
    #[error("The parity byte of the recovery key doesn't match: expected {0:?}, got {1:?}")]
    Parity(u8, u8),
    /// The recovery key has an invalid length.
    #[error("The decoded recovery key has a invalid length: expected {0}, got {1}")]
    Length(usize, usize),
    /// The recovry key isn't valid base58.
    #[error(transparent)]
    Base58(#[from] bs58::decode::Error),
    /// The  recovery key isn't valid base64.
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
    /// The recovery key is too short, we couldn't read enough data.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum UnpicklingError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("Couldn't decrypt the pickle: {0}")]
    Decryption(String),
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

/// The private part of a backup key.
#[derive(Zeroize)]
pub struct RecoveryKey {
    inner: [u8; RecoveryKey::KEY_SIZE],
}

impl Drop for RecoveryKey {
    fn drop(&mut self) {
        self.inner.zeroize()
    }
}

impl std::fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryKey").finish()
    }
}

/// The pickled version of a recovery key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickledRecoveryKey(String);

impl AsRef<str> for PickledRecoveryKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InnerPickle {
    version: u8,
    nonce: String,
    ciphertext: String,
}

impl TryFrom<String> for RecoveryKey {
    type Error = DecodeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_base58(&value)
    }
}

impl std::fmt::Display for RecoveryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let string = Zeroizing::new(self.to_base58());

        let string = Zeroizing::new(
            string
                .chars()
                .collect::<Vec<char>>()
                .chunks(Self::DISPLAY_CHUNK_SIZE)
                .map(|c| c.iter().collect::<String>())
                .collect::<Vec<_>>()
                .join(" "),
        );

        write!(f, "{}", string.as_str())
    }
}

impl RecoveryKey {
    const KEY_SIZE: usize = 32;
    const PREFIX: [u8; 2] = [0x8b, 0x01];
    const PREFIX_PARITY: u8 = Self::PREFIX[0] ^ Self::PREFIX[1];
    const DISPLAY_CHUNK_SIZE: usize = 4;

    fn parity_byte(bytes: &[u8]) -> u8 {
        bytes.iter().fold(Self::PREFIX_PARITY, |acc, x| acc ^ x)
    }

    /// Create a new random recovery key.
    pub fn new() -> Result<Self, RandomError> {
        let mut rng = thread_rng();

        let mut key = [0u8; Self::KEY_SIZE];
        key.try_fill(&mut rng)?;

        Ok(Self { inner: key })
    }

    /// Create a new recovery key from the given byte array.
    ///
    /// **Warning**: You need to make sure that the byte array contains correct
    /// random data, either by using a random number generator or by using an
    /// exported version of a previously created [`RecoveryKey`].
    pub fn from_bytes(key: [u8; Self::KEY_SIZE]) -> Self {
        Self { inner: key }
    }

    /// Try to create a [`RecoveryKey`] from a base64 export of a `RecoveryKey`.
    pub fn from_base64(key: &str) -> Result<Self, DecodeError> {
        let decoded = Zeroizing::new(crate::utilities::decode(key)?);

        if decoded.len() != Self::KEY_SIZE {
            Err(DecodeError::Length(decoded.len(), Self::KEY_SIZE))
        } else {
            let mut key = [0u8; Self::KEY_SIZE];
            key.copy_from_slice(&decoded);

            Ok(Self::from_bytes(key))
        }
    }

    /// Export the `RecoveryKey` as a base64 encoded string.
    pub fn to_base64(&self) -> String {
        encode(self.inner)
    }

    /// Try to create a [`RecoveryKey`] from a base58 export of a `RecoveryKey`.
    pub fn from_base58(value: &str) -> Result<Self, DecodeError> {
        // Remove any whitespace we might have
        let value: String = value.chars().filter(|c| !c.is_whitespace()).collect();

        let decoded = bs58::decode(value).with_alphabet(bs58::Alphabet::BITCOIN).into_vec()?;
        let mut decoded = Cursor::new(decoded);

        let mut prefix = [0u8; 2];
        let mut key = [0u8; Self::KEY_SIZE];
        let mut expected_parity = [0u8; 1];

        decoded.read_exact(&mut prefix)?;
        decoded.read_exact(&mut key)?;
        decoded.read_exact(&mut expected_parity)?;

        let expected_parity = expected_parity[0];
        let parity = Self::parity_byte(key.as_ref());

        let _ = Zeroizing::new(decoded.into_inner());

        if prefix != Self::PREFIX {
            Err(DecodeError::Prefix(Self::PREFIX, prefix))
        } else if expected_parity != parity {
            Err(DecodeError::Parity(expected_parity, parity))
        } else {
            Ok(Self::from_bytes(key))
        }
    }

    /// Export the `RecoveryKey` as a base58 encoded string.
    pub fn to_base58(&self) -> String {
        let bytes = Zeroizing::new(
            [
                Self::PREFIX.as_ref(),
                self.inner.as_ref(),
                [Self::parity_byte(self.inner.as_ref())].as_ref(),
            ]
            .concat(),
        );

        bs58::encode(bytes.as_slice()).with_alphabet(bs58::Alphabet::BITCOIN).into_string()
    }

    fn get_pk_decrytpion(&self) -> OlmPkDecryption {
        OlmPkDecryption::from_bytes(self.inner.as_ref())
            .expect("Can't generate a libom PkDecryption object from our private key")
    }

    /// Extract the public key from this `RecoveryKey`.
    pub fn public_key(&self) -> MegolmV1BackupKey {
        let pk = self.get_pk_decrytpion();
        let public_key = MegolmV1BackupKey::new(pk.public_key(), None);
        public_key
    }

    /// Export this [`RecoveryKey`] as an encrypted pickle that can be safely
    /// stored.
    pub fn pickle(&self, pickle_key: &[u8]) -> PickledRecoveryKey {
        let key = GenericArray::from_slice(pickle_key);
        let cipher = Aes256Gcm::new(key);

        let mut nonce = vec![0u8; NONCE_SIZE];
        let mut rng = thread_rng();

        nonce.try_fill(&mut rng).expect("Can't generate random nocne to pickle the recovery key");
        let nonce = GenericArray::from_slice(nonce.as_slice());

        let ciphertext =
            cipher.encrypt(nonce, self.inner.as_ref()).expect("Can't encrypt recovery key");

        let ciphertext = encode_url_safe(ciphertext);

        let pickle =
            InnerPickle { version: 1, nonce: encode_url_safe(nonce.as_slice()), ciphertext };

        PickledRecoveryKey(serde_json::to_string(&pickle).expect("Can't encode pickled signing"))
    }

    /// Try to import a `RecoveryKey` from a previously exported pickle.
    pub fn from_pickle(
        pickle: PickledRecoveryKey,
        pickle_key: &[u8],
    ) -> Result<Self, UnpicklingError> {
        let pickled: InnerPickle = serde_json::from_str(pickle.as_ref())?;

        let key = GenericArray::from_slice(pickle_key);
        let cipher = Aes256Gcm::new(key);

        let nonce = decode_url_safe(pickled.nonce).map_err(DecodeError::from)?;
        let nonce = GenericArray::from_slice(&nonce);
        let ciphertext = &decode_url_safe(pickled.ciphertext).map_err(DecodeError::from)?;

        let decrypted = cipher
            .decrypt(nonce, ciphertext.as_slice())
            .map_err(|e| UnpicklingError::Decryption(e.to_string()))?;

        if decrypted.len() != Self::KEY_SIZE {
            Err(DecodeError::Length(decrypted.len(), Self::KEY_SIZE).into())
        } else {
            let mut key = [0u8; Self::KEY_SIZE];
            key.copy_from_slice(&decrypted);

            Ok(Self { inner: key })
        }
    }

    /// Try to decrypt the given ciphertext using this `RecoveryKey`.
    pub fn decrypt(
        &self,
        mac: String,
        ephemeral_key: String,
        ciphertext: String,
    ) -> Result<String, OlmPkDecryptionError> {
        let message = PkMessage::new(mac, ephemeral_key, ciphertext);
        let pk = self.get_pk_decrytpion();

        pk.decrypt(message)
    }
}

#[cfg(test)]
mod test {
    use super::{DecodeError, RecoveryKey};

    const TEST_KEY: [u8; 32] = [
        0x77, 0x07, 0x6D, 0x0A, 0x73, 0x18, 0xA5, 0x7D, 0x3C, 0x16, 0xC1, 0x72, 0x51, 0xB2, 0x66,
        0x45, 0xDF, 0x4C, 0x2F, 0x87, 0xEB, 0xC0, 0x99, 0x2A, 0xB1, 0x77, 0xFB, 0xA5, 0x1D, 0xB9,
        0x2C, 0x2A,
    ];

    #[test]
    fn base64_decoding() -> Result<(), DecodeError> {
        let key = RecoveryKey::new().expect("Can't create a new recovery key");

        let base64 = key.to_base64();
        let decoded_key = RecoveryKey::from_base64(&base64)?;
        assert_eq!(key.inner, decoded_key.inner, "The decode key does't match the original");

        RecoveryKey::from_base64("i").expect_err("The recovery key is too short");

        Ok(())
    }

    #[test]
    fn base58_decoding() -> Result<(), DecodeError> {
        let key = RecoveryKey::new().expect("Can't create a new recovery key");

        let base64 = key.to_base58();
        let decoded_key = RecoveryKey::from_base58(&base64)?;
        assert_eq!(key.inner, decoded_key.inner, "The decode key does't match the original");

        let test_key =
            RecoveryKey::from_base58("EsTcLW2KPGiFwKEA3As5g5c4BXwkqeeJZJV8Q9fugUMNUE4d")?;
        assert_eq!(test_key.inner, TEST_KEY, "The decoded recovery key doesn't match the test key");

        let test_key = RecoveryKey::from_base58(
            "EsTc LW2K PGiF wKEA 3As5 g5c4 BXwk qeeJ ZJV8 Q9fu gUMN UE4d",
        )?;
        assert_eq!(test_key.inner, TEST_KEY, "The decoded recovery key doesn't match the test key");

        RecoveryKey::from_base58("EsTc LW2K PGiF wKEA 3As5 g5c4 BXwk qeeJ ZJV8 Q9fu gUMN UE4e")
            .expect_err("Can't create a recovery key if the parity byte is invalid");

        Ok(())
    }
}
