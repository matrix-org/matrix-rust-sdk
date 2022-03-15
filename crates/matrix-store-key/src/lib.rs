// Copyright 2022 The Matrix.org Foundation C.I.C.
// Copyright 2021 Damir Jelić
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

#![doc = include_str!("../README.md")]
#![warn(missing_debug_implementations, missing_docs)]

use blake3::{derive_key, Hash};
use chacha20poly1305::{
    aead::{Aead, Error as EncryptionError, NewAead},
    Key as ChachaKey, XChaCha20Poly1305, XNonce,
};
use displaydoc::Display;
use hmac::Hmac;
use pbkdf2::pbkdf2;
use rand::{thread_rng, Error as RandomError, Fill};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroize;

const VERSION: u8 = 1;
const KDF_SALT_SIZE: usize = 32;
const XNONCE_SIZE: usize = 24;
#[cfg(not(test))]
const KDF_ROUNDS: u32 = 200_000;
#[cfg(test)]
const KDF_ROUNDS: u32 = 1000;

type MacKeySeed = [u8; 32];

/// Error type for the `StoreKey` operations.
#[derive(Debug, Display, thiserror::Error)]
pub enum Error {
    /// Failed to serialize or deserialize a value {0}
    Serialization(#[from] serde_json::Error),
    /// Error encrypting or decrypting a value {0}
    Encryption(#[from] EncryptionError),
    /// Coulnd't generate enough randomness for a cryptographic operation: {0}
    Random(#[from] RandomError),
    /// Unsupported ciphertext version, expected {0}, got {1}
    Version(u8, u8),
    /// The ciphertext had an invalid length, expected {0}, got {1}
    Length(usize, usize),
}

/// An encryption key that can be used to encrypt data for key/value stores.
///
/// # Examples
///
/// ```
/// # let example = || {
/// use matrix_sdk_store_key::StoreKey;
/// use serde_json::{json, value::Value};
///
/// let store_key = StoreKey::new()?;
///
/// // Export the store key and persist it in your key/value store
/// let export = store_key.export("secret-passphrase")?;
///
/// let value = json!({
///     "some": "data",
/// });
///
/// let encrypted = store_key.encrypt_value(&value)?;
/// let decrypted: Value = store_key.decrypt_value(&encrypted)?;
///
/// assert_eq!(value, decrypted);
/// # Result::<_, anyhow::Error>::Ok(()) };
/// ```
#[allow(missing_debug_implementations)]
pub struct StoreKey {
    inner: Keys,
}

impl StoreKey {
    /// Generate a new random store key.
    pub fn new() -> Result<Self, Error> {
        Ok(Self { inner: Keys::new()? })
    }

    /// Encrypt the store key using the given passphrase and export it.
    ///
    /// This method can be used to persist the `StoreKey` in the key/value store
    /// in a safe manner.
    ///
    /// The `StoreKey` can later on be restored using [`StoreKey::import`].
    ///
    /// # Arguments
    ///
    /// * `passphrase` - The passphrase that should be used to encrypt the
    /// store key.
    ///
    /// # Examples
    ///
    /// ```
    /// # let example = || {
    /// use matrix_sdk_store_key::StoreKey;
    /// use serde_json::json;
    ///
    /// let store_key = StoreKey::new()?;
    ///
    /// // Export the store key and persist it in your key/value store
    /// let export = store_key.export("secret-passphrase");
    ///
    /// // Save the export in your key/value store.
    /// # Result::<_, anyhow::Error>::Ok(()) };
    /// ```
    pub fn export(&self, passphrase: &str) -> Result<Vec<u8>, Error> {
        let mut rng = thread_rng();

        let mut salt = [0u8; KDF_SALT_SIZE];
        salt.try_fill(&mut rng)?;

        let key = StoreKey::expand_key(passphrase, &salt, KDF_ROUNDS);
        let key = ChachaKey::from_slice(key.as_ref());
        let cipher = XChaCha20Poly1305::new(key);

        let nonce = Keys::get_nonce()?;

        let mut keys = [0u8; 64];

        keys[0..32].copy_from_slice(self.inner.encryption_key.as_ref());
        keys[32..64].copy_from_slice(self.inner.mac_key_seed.as_ref());

        let ciphertext = cipher.encrypt(XNonce::from_slice(&nonce), keys.as_ref())?;

        keys.zeroize();

        let store_key = EncryptedStoreKey {
            kdf_info: KdfInfo::Pbkdf2ToChaCha20Poly1305 { rounds: KDF_ROUNDS, kdf_salt: salt },
            ciphertext_info: CipherTextInfo::ChaCha20Poly1305 { nonce, ciphertext },
        };

        Ok(serde_json::to_vec(&store_key).expect("Can't serialize the store key"))
    }

    /// Restore a store key from an encrypted export.
    ///
    /// # Arguments
    ///
    /// * `passphrase` - The passphrase that was used to encrypt the store key.
    ///
    /// * `encrypted` - The exported and encrypted version of the store key.
    ///
    /// # Examples
    ///
    /// ```
    /// # let example = || {
    /// use matrix_sdk_store_key::StoreKey;
    /// use serde_json::json;
    ///
    /// let store_key = StoreKey::new()?;
    ///
    /// // Export the store key and persist it in your key/value store
    /// let export = store_key.export("secret-passphrase")?;
    ///
    /// // This is now the same as `store_key`.
    /// let imported = StoreKey::import("secret-passphrase", &export)?;
    ///
    /// // Save the export in your key/value store.
    /// # Result::<_, anyhow::Error>::Ok(()) };
    /// ```
    pub fn import(passphrase: &str, encrypted: &[u8]) -> Result<Self, Error> {
        let encrypted: EncryptedStoreKey = serde_json::from_slice(encrypted)?;

        let key = match encrypted.kdf_info {
            KdfInfo::Pbkdf2ToChaCha20Poly1305 { rounds, kdf_salt } => {
                Self::expand_key(passphrase, &kdf_salt, rounds)
            }
        };

        let key = ChachaKey::from_slice(key.as_ref());

        let mut decrypted = match encrypted.ciphertext_info {
            CipherTextInfo::ChaCha20Poly1305 { nonce, ciphertext } => {
                let cipher = XChaCha20Poly1305::new(key);
                let nonce = XNonce::from_slice(&nonce);
                cipher.decrypt(nonce, ciphertext.as_ref())?
            }
        };

        if decrypted.len() != 64 {
            Err(Error::Length(64, decrypted.len()))
        } else {
            let mut encryption_key = Box::new([0u8; 32]);
            let mut mac_key_seed = Box::new([0u8; 32]);

            encryption_key.copy_from_slice(&decrypted[0..32]);
            mac_key_seed.copy_from_slice(&decrypted[32..64]);

            let keys = Keys { encryption_key, mac_key_seed };

            decrypted.zeroize();

            Ok(Self { inner: keys })
        }
    }

    /// Hash a key before it is inserted into the key/value store.
    ///
    /// # Arguments
    ///
    /// * `table_name` - The name of the key/value table this key will be
    /// inserted into. This can also contain additional unique data. It will be
    /// used to derive a table specific cryptographic key which will be used
    /// in a keyed hash function. This ensures data independence between the
    /// different tables of the key/value store.
    ///
    /// * `key` - The key that should be hashed before it is inserted into the
    /// key/value store.
    ///
    /// **Note**: a key that is hashed by this method can't be retrieved
    /// anymore.
    ///
    /// # Examples
    ///
    /// ```
    /// # let example = || {
    /// use matrix_sdk_store_key::StoreKey;
    /// use serde_json::json;
    ///
    /// let store_key = StoreKey::new()?;
    ///
    /// let key = "bulbasaur";
    ///
    /// // Hash the key so people don't know which pokemon we have collected.
    /// let hashed_key = store_key.hash_key("list-of-pokemon", key.as_ref());
    ///
    /// // It's now safe to insert the key into our key/value store.
    /// # Result::<_, anyhow::Error>::Ok(()) };
    /// ```
    pub fn hash_key(&self, table_name: &str, key: &[u8]) -> [u8; 32] {
        let mac_key = self.inner.get_mac_key_for_table(table_name);

        mac_key.mac(key).into()
    }

    /// Encrypt a value before it is inserted into the key/value store.
    ///
    /// A value can be decrypted using the [`StoreKey::decrypt_value()`] method.
    ///
    /// # Arguments
    ///
    /// * `value` - A value that should be encrypted, any value that implements
    /// `Serialize` can be given to this method. The value will be serialized as
    /// json before it is encrypted.
    ///
    /// # Examples
    ///
    /// ```
    /// # let example = || {
    /// use matrix_sdk_store_key::StoreKey;
    /// use serde_json::{json, value::Value};
    ///
    /// let store_key = StoreKey::new()?;
    ///
    /// let value = json!({
    ///     "some": "data",
    /// });
    ///
    /// let encrypted = store_key.encrypt_value(&value)?;
    /// let decrypted: Value = store_key.decrypt_value(&encrypted)?;
    ///
    /// assert_eq!(value, decrypted);
    /// # Result::<_, anyhow::Error>::Ok(()) };
    /// ```
    pub fn encrypt_value(&self, value: &impl Serialize) -> Result<Vec<u8>, Error> {
        let data = serde_json::to_vec(value)?;

        let nonce = Keys::get_nonce()?;
        let cipher = XChaCha20Poly1305::new(self.inner.encryption_key());

        let ciphertext = cipher.encrypt(XNonce::from_slice(&nonce), data.as_ref())?;

        Ok(serde_json::to_vec(&EncryptedValue { version: VERSION, ciphertext, nonce })?)
    }

    /// Decrypt a value after it was fetchetd from the key/value store.
    ///
    /// A value can be encrypted using the [`StoreKey::encrypt_value()`] method.
    ///
    /// # Arguments
    ///
    /// * `value` - The ciphertext of a value that should be decrypted.
    ///
    /// The method will deserialize the decrypted value into the expected type.
    ///
    /// # Examples
    ///
    /// ```
    /// # let example = || {
    /// use matrix_sdk_store_key::StoreKey;
    /// use serde_json::{json, value::Value};
    ///
    /// let store_key = StoreKey::new()?;
    ///
    /// let value = json!({
    ///     "some": "data",
    /// });
    ///
    /// let encrypted = store_key.encrypt_value(&value)?;
    /// let decrypted: Value = store_key.decrypt_value(&encrypted)?;
    ///
    /// assert_eq!(value, decrypted);
    /// # Result::<_, anyhow::Error>::Ok(()) };
    pub fn decrypt_value<T: for<'b> Deserialize<'b>>(&self, value: &[u8]) -> Result<T, Error> {
        let value: EncryptedValue = serde_json::from_slice(value)?;

        if value.version != VERSION {
            return Err(Error::Version(VERSION, value.version));
        }

        let cipher = XChaCha20Poly1305::new(self.inner.encryption_key());
        let nonce = XNonce::from_slice(&value.nonce);
        let plaintext = cipher.decrypt(nonce, value.ciphertext.as_ref())?;

        Ok(serde_json::from_slice(&plaintext)?)
    }

    /// Expand the given passphrase into a KEY_SIZE long key.
    fn expand_key(passphrase: &str, salt: &[u8], rounds: u32) -> Box<[u8; 32]> {
        let mut key = Box::new([0u8; 32]);
        pbkdf2::<Hmac<Sha256>>(passphrase.as_bytes(), salt, rounds, &mut *key);

        key
    }
}

struct MacKey(Box<[u8; 32]>);

impl MacKey {
    fn mac(&self, input: &[u8]) -> Hash {
        blake3::keyed_hash(&self.0, input)
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct EncryptedValue {
    version: u8,
    ciphertext: Vec<u8>,
    nonce: [u8; XNONCE_SIZE],
}

#[derive(Zeroize)]
#[zeroize(drop)]
struct Keys {
    encryption_key: Box<[u8; 32]>,
    mac_key_seed: Box<[u8; 32]>,
}

impl Keys {
    fn new() -> Result<Self, Error> {
        let mut encryption_key = Box::new([0u8; 32]);
        let mut mac_key_seed = Box::new([0u8; 32]);

        let mut rng = thread_rng();

        encryption_key.try_fill(&mut rng)?;
        mac_key_seed.try_fill(&mut rng)?;

        Ok(Self { encryption_key, mac_key_seed })
    }

    fn encryption_key(&self) -> &ChachaKey {
        ChachaKey::from_slice(self.encryption_key.as_slice())
    }

    fn mac_key_seed(&self) -> &MacKeySeed {
        &self.mac_key_seed
    }

    fn get_mac_key_for_table(&self, table_name: &str) -> MacKey {
        let mut key = MacKey(Box::new([0u8; 32]));
        let mut output = derive_key(table_name, self.mac_key_seed());

        key.0.copy_from_slice(&output);

        output.zeroize();

        key
    }

    fn get_nonce() -> Result<[u8; XNONCE_SIZE], RandomError> {
        let mut nonce = [0u8; XNONCE_SIZE];
        let mut rng = thread_rng();

        nonce.try_fill(&mut rng)?;

        Ok(nonce)
    }
}

/// Version specific info for the key derivation method that is used.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
enum KdfInfo {
    /// The PBKDF2 to Chacha key derivation variant.
    Pbkdf2ToChaCha20Poly1305 {
        /// The number of PBKDF rounds that were used when deriving the store
        /// key.
        rounds: u32,
        /// The salt that was used when the passphrase was expanded into a store
        /// key.
        kdf_salt: [u8; KDF_SALT_SIZE],
    },
}

/// Version specific info for encryption method that is used to encrypt our
/// store key.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
enum CipherTextInfo {
    /// A store key encrypted using the ChaCha20Poly1305 AEAD.
    ChaCha20Poly1305 {
        /// The nonce that was used to encrypt the ciphertext.
        nonce: [u8; XNONCE_SIZE],
        /// The encrypted store key.
        ciphertext: Vec<u8>,
    },
}

/// An encrypted version of our store key, this can be safely stored in a
/// database.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct EncryptedStoreKey {
    /// Info about the key derivation method that was used to expand the
    /// passphrase into an encryption key.
    pub kdf_info: KdfInfo,
    /// The ciphertext with it's accompanying additional data that is needed to
    /// decrypt the store key.
    pub ciphertext_info: CipherTextInfo,
}

#[cfg(test)]
mod test {
    use serde_json::{json, Value};

    use super::{Error, StoreKey};

    #[test]
    fn generating() {
        StoreKey::new().unwrap();
    }

    #[test]
    fn exporting_store_key() -> Result<(), Error> {
        let passphrase = "it's a secret to everybody";
        let store_key = StoreKey::new()?;

        let value = json!({
            "some": "data"
        });

        let encrypted_value = store_key.encrypt_value(&value)?;

        let encrypted = store_key.export(passphrase)?;
        let decrypted = StoreKey::import(passphrase, &encrypted)?;

        assert_eq!(store_key.inner.encryption_key, decrypted.inner.encryption_key);
        assert_eq!(store_key.inner.mac_key_seed, decrypted.inner.mac_key_seed);

        let decrypted_value: Value = decrypted.decrypt_value(&encrypted_value)?;

        assert_eq!(value, decrypted_value);

        Ok(())
    }

    #[test]
    fn encrypting_values() -> Result<(), Error> {
        let event = json!({
                "content": {
                "body": "Bee Gees - Stayin' Alive",
                "info": {
                    "duration": 2140786u32,
                    "mimetype": "audio/mpeg",
                    "size": 1563685u32
                },
                "msgtype": "m.audio",
                "url": "mxc://example.org/ffed755USFFxlgbQYZGtryd"
            },
        });

        let store_key = StoreKey::new()?;

        let encrypted = store_key.encrypt_value(&event)?;
        let decrypted: Value = store_key.decrypt_value(&encrypted)?;

        assert_eq!(event, decrypted);

        Ok(())
    }

    #[test]
    fn encrypting_keys() -> Result<(), Error> {
        let store_key = StoreKey::new()?;

        let first = store_key.hash_key("some_table", b"It's dangerous to go alone");
        let second = store_key.hash_key("some_table", b"It's dangerous to go alone");
        let third = store_key.hash_key("another_table", b"It's dangerous to go alone");
        let fourth = store_key.hash_key("another_table", b"It's dangerous to go alone");
        let fifth = store_key.hash_key("another_table", b"It's not dangerous to go alone");

        assert_eq!(first, second);
        assert_ne!(first, third);
        assert_eq!(third, fourth);
        assert_ne!(fourth, fifth);

        Ok(())
    }
}
