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
use olm_rs::pk::OlmPkDecryption;
use rand::{thread_rng, Error as RandomError, Fill};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

use super::MegolmV1BackupKey;
use crate::utilities::{decode_url_safe, encode, encode_url_safe};

const NONCE_SIZE: usize = 12;

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("The decoded recovery key has an invalid prefix: expected {0:?}, got {1:?}")]
    Prefix([u8; 2], [u8; 2]),
    #[error("The parity byte of the recovery key doesn't match: expected {0:?}, got {1:?}")]
    Parity(u8, u8),
    #[error("The decoded recovery key has a invalid length: expected {0}, got {1}")]
    Length(usize, usize),
    #[error(transparent)]
    Base58(#[from] bs58::decode::Error),
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
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

#[derive(Zeroize)]
#[zeroize(drop)]
#[allow(missing_debug_implementations)]
pub struct RecoveryKey {
    key: [u8; RecoveryKey::KEY_SIZE],
    version: Option<String>,
}

impl RecoveryKey {
    fn as_bytes(&self) -> &[u8] {
        &self.key
    }
}

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
    backup_version: Option<String>,
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

    pub fn new() -> Result<Self, RandomError> {
        let mut rng = thread_rng();

        let mut key = [0u8; Self::KEY_SIZE];
        key.try_fill(&mut rng)?;

        Ok(Self { key, version: None })
    }

    pub fn from_bytes(key: [u8; Self::KEY_SIZE]) -> Self {
        Self { key, version: None }
    }

    pub fn from_base64(key: String) -> Result<Self, DecodeError> {
        let decoded = Zeroizing::new(crate::utilities::decode(key)?);

        if decoded.len() != Self::KEY_SIZE {
            Err(DecodeError::Length(decoded.len(), Self::KEY_SIZE))
        } else {
            let mut key = [0u8; Self::KEY_SIZE];
            key.copy_from_slice(&decoded);

            Ok(Self { key, version: None })
        }
    }

    pub fn to_base64(&self) -> String {
        encode(self.key)
    }

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
            Ok(Self { key, version: None })
        }
    }

    pub fn set_version(&mut self, version: String) {
        self.version = Some(version)
    }

    pub fn public_key(&self) -> MegolmV1BackupKey {
        let pk = OlmPkDecryption::from_private_key(self.key.as_ref()).unwrap();
        let public_key = MegolmV1BackupKey::new(pk.public_key(), self.version.clone());
        public_key
    }

    pub fn to_base58(&self) -> String {
        let bytes = Zeroizing::new(
            [
                Self::PREFIX.as_ref(),
                self.key.as_ref(),
                [Self::parity_byte(self.key.as_ref())].as_ref(),
            ]
            .concat(),
        );

        bs58::encode(bytes.as_slice()).with_alphabet(bs58::Alphabet::BITCOIN).into_string()
    }

    pub fn pickle(&self, pickle_key: &[u8]) -> PickledRecoveryKey {
        let key = GenericArray::from_slice(pickle_key);
        let cipher = Aes256Gcm::new(key);

        let mut nonce = vec![0u8; NONCE_SIZE];
        let mut rng = thread_rng();

        nonce.try_fill(&mut rng).expect("Can't generate random nocne to pickle the recovery key");
        let nonce = GenericArray::from_slice(nonce.as_slice());

        let ciphertext =
            cipher.encrypt(nonce, self.key.as_ref()).expect("Can't encrypt recovery key");

        let ciphertext = encode_url_safe(ciphertext);

        let pickle = InnerPickle {
            version: 1,
            nonce: encode_url_safe(nonce.as_slice()),
            ciphertext,
            backup_version: self.version.clone(),
        };

        PickledRecoveryKey(serde_json::to_string(&pickle).expect("Can't encode pickled signing"))
    }

    pub fn from_pickle(
        &self,
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

            Ok(Self { key, version: pickled.backup_version })
        }
    }
}
