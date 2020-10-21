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

#![allow(dead_code)]

use aes_gcm::{
    aead::{generic_array::GenericArray, Aead, NewAead},
    Aes256Gcm,
};
use getrandom::getrandom;
use hmac::Hmac;
use pbkdf2::pbkdf2;
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

use serde::{Deserialize, Serialize};

const VERSION: u8 = 1;
const KEY_SIZE: usize = 32;
const NONCE_SIZE: usize = 12;
const KDF_SALT_SIZE: usize = 32;
const KDF_ROUNDS: u32 = 10000;

/// An encrypted version of our pickle key, this can be safely stored in a
/// database.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct EncryptedPickleKey {
    /// The version of the encrypted pickle.
    pub version: u8,
    /// The salt that was used when the passphrase was expanded into a AES key.
    pub kdf_salt: Vec<u8>,
    /// The nonce that was used to encrypt the pickle key.
    pub nonce: Vec<u8>,
    /// The encrypted pickle key.
    pub ciphertext: Vec<u8>,
}

/// A pickle key that will be used to encrypt all the private keys for Olm.
///
/// Olm uses AES256 to encrypt accounts, sessions, inbound group sessions. We
/// also implement our own pickling for the cross-signing types using
/// AES256-GCM so the key sizes match.
#[derive(Debug, Zeroize, PartialEq)]
pub struct PickleKey {
    version: u8,
    aes256_key: Vec<u8>,
}

impl PickleKey {
    /// Generate a new random pickle key.
    pub fn new() -> Self {
        let mut key = vec![0u8; KEY_SIZE];
        getrandom(&mut key).expect("Can't generate new pickle key");

        Self {
            version: VERSION,
            aes256_key: key,
        }
    }

    fn expand_key(passphrase: &str, salt: &[u8]) -> Zeroizing<Vec<u8>> {
        let mut key = Zeroizing::from(vec![0u8; KEY_SIZE]);
        pbkdf2::<Hmac<Sha256>>(passphrase.as_bytes(), &salt, KDF_ROUNDS, &mut *key);
        key
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

        let key = PickleKey::expand_key(passphrase, &salt);
        let key = GenericArray::from_slice(key.as_ref());
        let cipher = Aes256Gcm::new(&key);

        let mut nonce = vec![0u8; NONCE_SIZE];
        getrandom(&mut nonce).expect("Can't generate new random nonce for the pickle key");

        let ciphertext = cipher
            .encrypt(
                &GenericArray::from_slice(nonce.as_ref()),
                self.aes256_key.as_slice(),
            )
            .expect("Can't encrypt pickle key");

        EncryptedPickleKey {
            version: self.version,
            kdf_salt: salt,
            nonce,
            ciphertext,
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
    pub fn from_encrypted(passphrase: &str, encrypted: EncryptedPickleKey) -> Self {
        let key = PickleKey::expand_key(passphrase, &encrypted.kdf_salt);
        let key = GenericArray::from_slice(key.as_ref());
        let cipher = Aes256Gcm::new(&key);
        let nonce = GenericArray::from_slice(&encrypted.nonce);

        let decrypted_key = cipher
            .decrypt(nonce, encrypted.ciphertext.as_ref())
            .expect("Can't decrypt pickle");

        Self {
            version: encrypted.version,
            aes256_key: decrypted_key,
        }
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
        let decrypted = PickleKey::from_encrypted(passphrase, encrypted);

        assert_eq!(pickle_key, decrypted);
    }
}
