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

//! Facilities for StateStore implementations to reuse to manage encrypted
//! state keys

use std::convert::TryFrom;

use chacha20poly1305::{
    aead::{Aead, Error as EncryptionError, NewAead},
    ChaCha20Poly1305, Key, Nonce, XChaCha20Poly1305, XNonce,
};
use hmac::Hmac;
use pbkdf2::pbkdf2;
use rand::{thread_rng, Error as RngError, Fill};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

use crate::StoreError;

const VERSION: u8 = 1;
const KEY_SIZE: usize = 32;
const NONCE_SIZE: usize = 12;
const XNONCE_SIZE: usize = 24;
const KDF_SALT_SIZE: usize = 32;
#[cfg(not(test))]
const KDF_ROUNDS: u32 = 200_000;
#[cfg(test)]
const KDF_ROUNDS: u32 = 1000;

/// Local State Error for Store Keys
///
/// provides facilities to directly convert into a `StoreError` thus the most
/// common usage is to `.map_err(StoreError::into)` it.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A problem de/serializing the JSON
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    /// A problem with en/decrypting
    #[error("Error encrypting or decrypting an event {0}")]
    Encryption(String),
    /// Error generating enough random data for a cryptographic operation
    #[error("Error generating enough random data for a cryptographic operation")]
    Random(#[from] RngError),
}

#[allow(clippy::from_over_into)]
impl Into<StoreError> for Error {
    fn into(self) -> StoreError {
        match self {
            Error::Serialization(e) => StoreError::Json(e),
            Error::Encryption(e) => StoreError::Encryption(e),
            Error::Random(_) => StoreError::Encryption(self.to_string()),
        }
    }
}

impl From<EncryptionError> for Error {
    fn from(e: EncryptionError) -> Self {
        Error::Encryption(e.to_string())
    }
}

/// Holding the  data of the encrypted event
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct EncryptedEvent {
    version: u8,
    ciphertext: Vec<u8>,
    nonce: Vec<u8>,
}

/// Version specific info for the key derivation method that is used.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
enum KdfInfo {
    Pbkdf2ToChaCha20Poly1305 {
        /// The number of PBKDF rounds that were used when deriving the store
        /// key.
        rounds: u32,
        /// The salt that was used when the passphrase was expanded into a store
        /// key.
        kdf_salt: Vec<u8>,
    },
}

/// Version specific info for encryption method that is used to encrypt our
/// store key.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
enum CipherTextInfo {
    ChaCha20Poly1305 {
        /// The nonce that was used to encrypt the ciphertext.
        nonce: Vec<u8>,
        /// The encrypted store key.
        ciphertext: Vec<u8>,
    },
}

/// An encrypted version of our store key, this can be safely stored in a
/// database.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct EncryptedStoreKey {
    /// Info about the key derivation method that was used to expand the
    /// passphrase into an encryption key.
    kdf_info: KdfInfo,
    /// The ciphertext with it's accompanying additional data that is needed to
    /// decrypt the store key.
    ciphertext_info: CipherTextInfo,
}

/// A store key that can be used to encrypt entries in the store.
#[derive(Debug, Zeroize, PartialEq)]
pub struct StoreKey {
    inner: Vec<u8>,
}

impl TryFrom<Vec<u8>> for StoreKey {
    type Error = ();

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        if value.len() != KEY_SIZE {
            Err(())
        } else {
            Ok(Self { inner: value })
        }
    }
}

impl StoreKey {
    /// Generate a new random store key.
    pub fn new() -> Result<Self, Error> {
        let mut key = vec![0u8; KEY_SIZE];
        let mut rng = thread_rng();
        key.try_fill(&mut rng)?;

        Ok(Self { inner: key })
    }

    /// Expand the given passphrase into a KEY_SIZE long key.
    fn expand_key(passphrase: &str, salt: &[u8], rounds: u32) -> Zeroizing<Vec<u8>> {
        let mut key = Zeroizing::from(vec![0u8; KEY_SIZE]);
        pbkdf2::<Hmac<Sha256>>(passphrase.as_bytes(), salt, rounds, &mut *key);
        key
    }

    /// Get the store key.
    fn key(&self) -> &Key {
        Key::from_slice(&self.inner)
    }

    /// Encrypt and export our store key using the given passphrase.
    ///
    /// # Arguments
    ///
    /// * `passphrase` - The passphrase that should be used to encrypt the
    /// store key.
    pub fn export(&self, passphrase: &str) -> Result<EncryptedStoreKey, Error> {
        let mut rng = thread_rng();

        let mut salt = vec![0u8; KDF_SALT_SIZE];
        salt.try_fill(&mut rng)?;

        let key = StoreKey::expand_key(passphrase, &salt, KDF_ROUNDS);
        let key = Key::from_slice(key.as_ref());
        let cipher = ChaCha20Poly1305::new(key);

        let mut nonce = vec![0u8; NONCE_SIZE];
        nonce.try_fill(&mut rng)?;

        let ciphertext =
            cipher.encrypt(Nonce::from_slice(nonce.as_ref()), self.inner.as_slice())?;

        Ok(EncryptedStoreKey {
            kdf_info: KdfInfo::Pbkdf2ToChaCha20Poly1305 { rounds: KDF_ROUNDS, kdf_salt: salt },
            ciphertext_info: CipherTextInfo::ChaCha20Poly1305 { nonce, ciphertext },
        })
    }

    fn get_nonce() -> Result<Vec<u8>, RngError> {
        let mut nonce = vec![0u8; XNONCE_SIZE];
        let mut rng = thread_rng();

        nonce.try_fill(&mut rng)?;

        Ok(nonce)
    }

    /// Encrypt the given Event after serializing it with serde_json
    pub fn encrypt(&self, event: &impl Serialize) -> Result<EncryptedEvent, Error> {
        let event = serde_json::to_vec(event)?;

        let nonce = StoreKey::get_nonce()?;
        let cipher = XChaCha20Poly1305::new(self.key());
        let xnonce = XNonce::from_slice(&nonce);

        let ciphertext = cipher.encrypt(xnonce, event.as_ref())?;

        Ok(EncryptedEvent { version: VERSION, ciphertext, nonce })
    }

    /// Decrypt the given encrypted event back into the inner
    pub fn decrypt<T: for<'b> Deserialize<'b>>(&self, event: EncryptedEvent) -> Result<T, Error> {
        if event.version != VERSION {
            return Err(Error::Encryption(
                "Error decrypting: Unknown ciphertext version".to_string(),
            ));
        }

        let cipher = XChaCha20Poly1305::new(self.key());
        let nonce = XNonce::from_slice(&event.nonce);
        let plaintext = cipher.decrypt(nonce, event.ciphertext.as_ref())?;

        Ok(serde_json::from_slice(&plaintext)?)
    }

    /// Restore a store key from an encrypted export.
    ///
    /// # Arguments
    ///
    /// * `passphrase` - The passphrase that should be used to encrypt the
    /// store key.
    ///
    /// * `encrypted` - The exported and encrypted version of the store key.
    pub fn import(passphrase: &str, encrypted: EncryptedStoreKey) -> Result<Self, EncryptionError> {
        let key = match encrypted.kdf_info {
            KdfInfo::Pbkdf2ToChaCha20Poly1305 { rounds, kdf_salt } => {
                Self::expand_key(passphrase, &kdf_salt, rounds)
            }
        };

        let key = Key::from_slice(key.as_ref());

        let decrypted = match encrypted.ciphertext_info {
            CipherTextInfo::ChaCha20Poly1305 { nonce, ciphertext } => {
                let cipher = ChaCha20Poly1305::new(key);
                let nonce = Nonce::from_slice(&nonce);
                cipher.decrypt(nonce, ciphertext.as_ref())?
            }
        };

        Ok(Self { inner: decrypted })
    }
}

#[cfg(test)]
mod test {
    use serde_json::{json, Value};

    use super::StoreKey;

    #[test]
    fn generating() {
        StoreKey::new().unwrap();
    }

    #[test]
    fn encrypting() {
        let passphrase = "it's a secret to everybody";
        let store_key = StoreKey::new().unwrap();

        let encrypted = store_key.export(passphrase).unwrap();
        let decrypted = StoreKey::import(passphrase, encrypted).unwrap();

        assert_eq!(store_key, decrypted);
    }

    #[test]
    fn encrypting_events() {
        let event = json!({
                "content": {
                "body": "Bee Gees - Stayin' Alive",
                "info": {
                    "duration": 2140786,
                    "mimetype": "audio/mpeg",
                    "size": 1563685
                },
                "msgtype": "m.audio",
                "url": "mxc://example.org/ffed755USFFxlgbQYZGtryd"
            },
        });

        let store_key = StoreKey::new().unwrap();

        let encrypted = store_key.encrypt(&event).unwrap();
        let decrypted: Value = store_key.decrypt(encrypted).unwrap();
        assert_eq!(event, decrypted);
    }
}
