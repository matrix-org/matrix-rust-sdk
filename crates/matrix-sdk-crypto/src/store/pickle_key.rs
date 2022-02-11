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

use std::convert::TryFrom;

use aes_gcm::{
    aead::{generic_array::GenericArray, Aead, NewAead},
    Aes256Gcm, Error as DecryptionError,
};
use getrandom::getrandom;
use hmac::Hmac;
use pbkdf2::pbkdf2;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

const KEY_SIZE: usize = 32;
const NONCE_SIZE: usize = 12;
const KDF_SALT_SIZE: usize = 32;
#[cfg(not(test))]
const KDF_ROUNDS: u32 = 200_000;
#[cfg(test)]
const KDF_ROUNDS: u32 = 1000;

/// Version specific info for the key derivation method that is used.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub enum KdfInfo {
    Pbkdf2 {
        /// The number of PBKDF rounds that were used when deriving the AES key.
        rounds: u32,
    },
}

/// Version specific info for encryption method that is used to encrypt our
/// pickle key.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub enum CipherTextInfo {
    Aes256Gcm {
        /// The nonce that was used to encrypt the ciphertext.
        nonce: Vec<u8>,
        /// The encrypted pickle key.
        ciphertext: Vec<u8>,
    },
}

/// An encrypted version of our pickle key, this can be safely stored in a
/// database.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct EncryptedPickleKey {
    /// Info about the key derivation method that was used to expand the
    /// passphrase into an encryption key.
    pub kdf_info: KdfInfo,
    /// The ciphertext with it's accompanying additional data that is needed to
    /// decrypt the pickle key.
    pub ciphertext_info: CipherTextInfo,
    /// The salt that was used when the passphrase was expanded into a AES key.
    kdf_salt: Vec<u8>,
}

/// A pickle key that will be used to encrypt all the private keys for Olm.
///
/// Olm uses AES256 to encrypt accounts, sessions, inbound group sessions. We
/// also implement our own pickling for the cross-signing types using
/// AES256-GCM so the key sizes match.
#[derive(Debug, Zeroize, PartialEq)]
pub struct PickleKey {
    aes256_key: Vec<u8>,
}

impl Default for PickleKey {
    fn default() -> Self {
        let mut key = vec![0u8; KEY_SIZE];
        getrandom(&mut key).expect("Can't generate new pickle key");

        Self { aes256_key: key }
    }
}

impl TryFrom<Vec<u8>> for PickleKey {
    type Error = ();
    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        if value.len() != KEY_SIZE {
            Err(())
        } else {
            Ok(Self { aes256_key: value })
        }
    }
}

impl PickleKey {
    /// Generate a new random pickle key.
    pub fn new() -> Self {
        Default::default()
    }

    fn expand_key(passphrase: &str, salt: &[u8], rounds: u32) -> Zeroizing<Vec<u8>> {
        let mut key = Zeroizing::from(vec![0u8; KEY_SIZE]);
        pbkdf2::<Hmac<Sha256>>(passphrase.as_bytes(), salt, rounds, &mut *key);
        key
    }

    /// Get the raw AES256 key.
    pub fn key(&self) -> &[u8] {
        &self.aes256_key
    }

    /// Encrypt and export our pickle key using the given passphrase.
    ///
    /// # Arguments
    ///
    /// * `passphrase` - The passphrase that should be used to encrypt the
    /// pickle key.
    pub fn encrypt(&self, passphrase: &str) -> EncryptedPickleKey {
        let mut salt = vec![0u8; KDF_SALT_SIZE];
        getrandom(&mut salt).expect("Can't generate new random pickle key");

        let key = PickleKey::expand_key(passphrase, &salt, KDF_ROUNDS);
        let key = GenericArray::from_slice(key.as_ref());
        let cipher = Aes256Gcm::new(key);

        let mut nonce = vec![0u8; NONCE_SIZE];
        getrandom(&mut nonce).expect("Can't generate new random nonce for the pickle key");

        let ciphertext = cipher
            .encrypt(GenericArray::from_slice(nonce.as_ref()), self.aes256_key.as_slice())
            .expect("Can't encrypt pickle key");

        EncryptedPickleKey {
            kdf_info: KdfInfo::Pbkdf2 { rounds: KDF_ROUNDS },
            kdf_salt: salt,
            ciphertext_info: CipherTextInfo::Aes256Gcm { nonce, ciphertext },
        }
    }

    /// Restore a pickle key from an encrypted export.
    ///
    /// # Arguments
    ///
    /// * `passphrase` - The passphrase that should be used to encrypt the
    /// pickle key.
    ///
    /// * `encrypted` - The exported and encrypted version of the pickle key.
    pub fn from_encrypted(
        passphrase: &str,
        encrypted: EncryptedPickleKey,
    ) -> Result<Self, DecryptionError> {
        let key = match encrypted.kdf_info {
            KdfInfo::Pbkdf2 { rounds } => Self::expand_key(passphrase, &encrypted.kdf_salt, rounds),
        };

        let key = GenericArray::from_slice(key.as_ref());

        let decrypted = match encrypted.ciphertext_info {
            CipherTextInfo::Aes256Gcm { nonce, ciphertext } => {
                let cipher = Aes256Gcm::new(key);
                let nonce = GenericArray::from_slice(&nonce);
                cipher.decrypt(nonce, ciphertext.as_ref())?
            }
        };

        Ok(Self { aes256_key: decrypted })
    }
}

#[cfg(test)]
mod test {
    use super::PickleKey;

    #[test]
    fn generating() {
        PickleKey::new();
    }

    #[test]
    fn encrypting() {
        let passphrase = "it's a secret to everybody";
        let pickle_key = PickleKey::new();

        let encrypted = pickle_key.encrypt(passphrase);
        let decrypted = PickleKey::from_encrypted(passphrase, encrypted).unwrap();

        assert_eq!(pickle_key, decrypted);
    }
}
