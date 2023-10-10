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

use std::ops::DerefMut;

use blake3::{derive_key, Hash};
use chacha20poly1305::{
    aead::{Aead, Error as EncryptionError},
    Key as ChachaKey, KeyInit, XChaCha20Poly1305, XNonce,
};
use displaydoc::Display;
use hmac::Hmac;
use pbkdf2::pbkdf2;
use rand::{thread_rng, Error as RandomError, Fill};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroize;

const VERSION: u8 = 1;
const KDF_SALT_SIZE: usize = 32;
const XNONCE_SIZE: usize = 24;
const KDF_ROUNDS: u32 = 200_000;

type MacKeySeed = [u8; 32];

/// Error type for the `StoreCipher` operations.
#[derive(Debug, Display, thiserror::Error)]
pub enum Error {
    /// Failed to serialize a value {0}
    Serialization(#[from] rmp_serde::encode::Error),
    /// Failed to deserialize a value {0}
    Deserialization(#[from] rmp_serde::decode::Error),
    /// Failed to deserialize or serialize a JSON value {0}
    Json(#[from] serde_json::Error),
    /// Error encrypting or decrypting a value {0}
    Encryption(#[from] EncryptionError),
    /// Coulnd't generate enough randomness for a cryptographic operation: {0}
    Random(#[from] RandomError),
    /// Unsupported ciphertext version, expected {0}, got {1}
    Version(u8, u8),
    /// The ciphertext had an invalid length, expected {0}, got {1}
    Length(usize, usize),
    /**
     * Failed to import a store cipher, the export used a passphrase while
     * we're trying to import it using a key or vice-versa.
     */
    KdfMismatch,
}

/// An encryption key that can be used to encrypt data for key/value stores.
///
/// # Examples
///
/// ```
/// # let example = || {
/// use matrix_sdk_store_encryption::StoreCipher;
/// use serde_json::{json, value::Value};
///
/// let store_cipher = StoreCipher::new()?;
///
/// // Export the store cipher and persist it in your key/value store
/// let export = store_cipher.export("secret-passphrase")?;
///
/// let value = json!({
///     "some": "data",
/// });
///
/// let encrypted = store_cipher.encrypt_value(&value)?;
/// let decrypted: Value = store_cipher.decrypt_value(&encrypted)?;
///
/// assert_eq!(value, decrypted);
/// # anyhow::Ok(()) };
/// ```
#[allow(missing_debug_implementations)]
pub struct StoreCipher {
    inner: Keys,
}

impl StoreCipher {
    /// Generate a new random store cipher.
    pub fn new() -> Result<Self, Error> {
        Ok(Self { inner: Keys::new()? })
    }

    /// Encrypt the store cipher using the given passphrase and export it.
    ///
    /// This method can be used to persist the `StoreCipher` in an unencrypted
    /// key/value store in a safe manner.
    ///
    /// The `StoreCipher` can later on be restored using
    /// [`StoreCipher::import`].
    ///
    /// # Arguments
    ///
    /// * `passphrase` - The passphrase that should be used to encrypt the
    /// store cipher.
    ///
    /// # Examples
    ///
    /// ```
    /// # let example = || {
    /// use matrix_sdk_store_encryption::StoreCipher;
    /// use serde_json::json;
    ///
    /// let store_cipher = StoreCipher::new()?;
    ///
    /// // Export the store cipher and persist it in your key/value store
    /// let export = store_cipher.export("secret-passphrase");
    ///
    /// // Save the export in your key/value store.
    /// # anyhow::Ok(()) };
    /// ```
    pub fn export(&self, passphrase: &str) -> Result<Vec<u8>, Error> {
        self.export_kdf(passphrase, KDF_ROUNDS)
    }

    /// Encrypt the store cipher using the given key and export it.
    ///
    /// This method can be used to persist the `StoreCipher` in an unencrypted
    /// key/value store in a safe manner.
    ///
    /// The `StoreCipher` can later on be restored using
    /// [`StoreCipher::import_with_key`].
    ///
    /// # Arguments
    ///
    /// * `key` - The 32-byte key to be used to encrypt the store cipher. It's
    /// recommended to use a freshly and securely generated random key.
    ///
    /// # Examples
    ///
    /// ```
    /// # let example = || {
    /// use matrix_sdk_store_encryption::StoreCipher;
    /// use serde_json::json;
    ///
    /// let store_cipher = StoreCipher::new()?;
    ///
    /// // Export the store cipher and persist it in your key/value store
    /// let export = store_cipher.export_with_key(&[0u8; 32]);
    ///
    /// // Save the export in your key/value store.
    /// # anyhow::Ok(()) };
    /// ```
    pub fn export_with_key(&self, key: &[u8; 32]) -> Result<Vec<u8>, Error> {
        let store_cipher = self.export_helper(key, KdfInfo::None)?;
        Ok(rmp_serde::to_vec_named(&store_cipher).expect("Can't serialize the store cipher"))
    }

    fn export_helper(
        &self,
        key: &[u8; 32],
        kdf_info: KdfInfo,
    ) -> Result<EncryptedStoreCipher, Error> {
        let key = ChachaKey::from_slice(key.as_ref());
        let cipher = XChaCha20Poly1305::new(key);

        let nonce = Keys::get_nonce()?;

        let mut keys = [0u8; 64];

        keys[0..32].copy_from_slice(self.inner.encryption_key.as_ref());
        keys[32..64].copy_from_slice(self.inner.mac_key_seed.as_ref());

        let ciphertext = cipher.encrypt(XNonce::from_slice(&nonce), keys.as_ref())?;

        keys.zeroize();

        Ok(EncryptedStoreCipher {
            kdf_info,
            ciphertext_info: CipherTextInfo::ChaCha20Poly1305 { nonce, ciphertext },
        })
    }

    #[doc(hidden)]
    pub fn _insecure_export_fast_for_testing(&self, passphrase: &str) -> Result<Vec<u8>, Error> {
        self.export_kdf(passphrase, 1000)
    }

    fn export_kdf(&self, passphrase: &str, kdf_rounds: u32) -> Result<Vec<u8>, Error> {
        let mut rng = thread_rng();

        let mut salt = [0u8; KDF_SALT_SIZE];
        salt.try_fill(&mut rng)?;

        let key = StoreCipher::expand_key(passphrase, &salt, kdf_rounds);

        let store_cipher = self.export_helper(
            &key,
            KdfInfo::Pbkdf2ToChaCha20Poly1305 { rounds: kdf_rounds, kdf_salt: salt },
        )?;

        Ok(rmp_serde::to_vec_named(&store_cipher).expect("Can't serialize the store cipher"))
    }

    fn import_helper(key: &ChachaKey, encrypted: EncryptedStoreCipher) -> Result<Self, Error> {
        let mut decrypted = match encrypted.ciphertext_info {
            CipherTextInfo::ChaCha20Poly1305 { nonce, ciphertext } => {
                let cipher = XChaCha20Poly1305::new(key);
                let nonce = XNonce::from_slice(&nonce);
                cipher.decrypt(nonce, ciphertext.as_ref())?
            }
        };

        if decrypted.len() != 64 {
            decrypted.zeroize();

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

    /// Restore a store cipher from an export encrypted with a passphrase.
    ///
    /// # Arguments
    ///
    /// * `passphrase` - The passphrase that was used to encrypt the store
    /// cipher.
    ///
    /// * `encrypted` - The exported and encrypted version of the store cipher.
    ///
    /// # Examples
    ///
    /// ```
    /// # let example = || {
    /// use matrix_sdk_store_encryption::StoreCipher;
    /// use serde_json::json;
    ///
    /// let store_cipher = StoreCipher::new()?;
    ///
    /// // Export the store cipher and persist it in your key/value store
    /// let export = store_cipher.export("secret-passphrase")?;
    ///
    /// // This is now the same as `store_cipher`.
    /// let imported = StoreCipher::import("secret-passphrase", &export)?;
    ///
    /// // Save the export in your key/value store.
    /// # anyhow::Ok(()) };
    /// ```
    pub fn import(passphrase: &str, encrypted: &[u8]) -> Result<Self, Error> {
        // Our old export format used serde_json for the serialization format. Let's
        // first try the new format and if that fails, try the old one.
        let encrypted: EncryptedStoreCipher =
            if let Ok(deserialized) = rmp_serde::from_slice(encrypted) {
                deserialized
            } else {
                serde_json::from_slice(encrypted)?
            };

        let key = match encrypted.kdf_info {
            KdfInfo::Pbkdf2ToChaCha20Poly1305 { rounds, kdf_salt } => {
                Self::expand_key(passphrase, &kdf_salt, rounds)
            }
            KdfInfo::None => {
                return Err(Error::KdfMismatch);
            }
        };

        let key = ChachaKey::from_slice(key.as_ref());

        Self::import_helper(key, encrypted)
    }

    /// Restore a store cipher from an export encrypted with a random key.
    ///
    /// # Arguments
    ///
    /// * `key` - The 32-byte decryption key that was previously used to
    /// encrypt the store cipher.
    ///
    /// * `encrypted` - The exported and encrypted version of the store cipher.
    ///
    /// # Examples
    ///
    /// ```
    /// # let example = || {
    /// use matrix_sdk_store_encryption::StoreCipher;
    /// use serde_json::json;
    ///
    /// let store_cipher = StoreCipher::new()?;
    ///
    /// // Export the store cipher and persist it in your key/value store
    /// let export = store_cipher.export_with_key(&[0u8; 32])?;
    ///
    /// // This is now the same as `store_cipher`.
    /// let imported = StoreCipher::import_with_key(&[0u8; 32], &export)?;
    ///
    /// // Save the export in your key/value store.
    /// # anyhow::Ok(()) };
    /// ```
    pub fn import_with_key(key: &[u8; 32], encrypted: &[u8]) -> Result<Self, Error> {
        let encrypted: EncryptedStoreCipher = rmp_serde::from_slice(encrypted).unwrap();

        if let KdfInfo::Pbkdf2ToChaCha20Poly1305 { .. } = encrypted.kdf_info {
            return Err(Error::KdfMismatch);
        };

        let key = ChachaKey::from_slice(key.as_ref());

        Self::import_helper(key, encrypted)
    }

    /// Hash a key before it is inserted into the key/value store.
    ///
    /// This prevents the key names from leaking to parties which do not have
    /// the ability to decrypt the key/value store.
    ///
    /// # Arguments
    ///
    /// * `table_name` - The name of the key/value table this key will be
    /// inserted into. This can also contain additional unique data. It will be
    /// used to derive a table-specific cryptographic key which will be used
    /// in a keyed hash function. This ensures data independence between the
    /// different tables of the key/value store.
    ///
    /// * `key` - The key to be hashed, prior to insertion into the key/value
    ///   store.
    ///
    /// **Note**: This is a one-way transformation; you cannot obtain the
    /// original key from its hash.
    ///
    /// # Examples
    ///
    /// ```
    /// # let example = || {
    /// use matrix_sdk_store_encryption::StoreCipher;
    /// use serde_json::json;
    ///
    /// let store_cipher = StoreCipher::new()?;
    ///
    /// let key = "bulbasaur";
    ///
    /// // Hash the key so people don't know which pokemon we have collected.
    /// let hashed_key = store_cipher.hash_key("list-of-pokemon", key.as_ref());
    ///
    /// // It's now safe to insert the key into our key/value store.
    /// # anyhow::Ok(()) };
    /// ```
    pub fn hash_key(&self, table_name: &str, key: &[u8]) -> [u8; 32] {
        let mac_key = self.inner.get_mac_key_for_table(table_name);

        mac_key.mac(key).into()
    }

    /// Encrypt a value before it is inserted into the key/value store.
    ///
    /// A value can be decrypted using the [`StoreCipher::decrypt_value()`]
    /// method.
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
    /// use matrix_sdk_store_encryption::StoreCipher;
    /// use serde_json::{json, value::Value};
    ///
    /// let store_cipher = StoreCipher::new()?;
    ///
    /// let value = json!({
    ///     "some": "data",
    /// });
    ///
    /// let encrypted = store_cipher.encrypt_value(&value)?;
    /// let decrypted: Value = store_cipher.decrypt_value(&encrypted)?;
    ///
    /// assert_eq!(value, decrypted);
    /// # anyhow::Ok(()) };
    /// ```
    pub fn encrypt_value(&self, value: &impl Serialize) -> Result<Vec<u8>, Error> {
        Ok(serde_json::to_vec(&self.encrypt_value_typed(value)?)?)
    }

    /// Encrypt a value before it is inserted into the key/value store.
    ///
    /// A value can be decrypted using the
    /// [`StoreCipher::decrypt_value_typed()`] method. This is the lower
    /// level function to `encrypt_value`, but returns the
    /// full `EncryptdValue`-type
    ///
    /// # Arguments
    ///
    /// * `value` - A value that should be encrypted, any value that implements
    /// `Serialize` can be given to this method. The value will be serialized as
    /// json before it is encrypted.
    ///
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # let example = || {
    /// use matrix_sdk_store_encryption::StoreCipher;
    /// use serde_json::{json, value::Value};
    ///
    /// let store_cipher = StoreCipher::new()?;
    ///
    /// let value = json!({
    ///     "some": "data",
    /// });
    ///
    /// let encrypted = store_cipher.encrypt_value_typed(&value)?;
    /// let decrypted: Value = store_cipher.decrypt_value_typed(encrypted)?;
    ///
    /// assert_eq!(value, decrypted);
    /// # anyhow::Ok(()) };
    /// ```
    pub fn encrypt_value_typed(&self, value: &impl Serialize) -> Result<EncryptedValue, Error> {
        let data = serde_json::to_vec(value)?;
        self.encrypt_value_data(data)
    }

    /// Encrypt some data before it is inserted into the key/value store.
    ///
    /// A value can be decrypted using the [`StoreCipher::decrypt_value_data()`]
    /// method. This is the lower level function to `encrypt_value`
    ///
    /// # Arguments
    ///
    /// * `data` - A value that should be encrypted, encoded as a `Vec<u8>`
    ///
    /// # Examples
    ///
    /// ```
    /// # let example = || {
    /// use matrix_sdk_store_encryption::StoreCipher;
    /// use serde_json::{json, value::Value};
    ///
    /// let store_cipher = StoreCipher::new()?;
    ///
    /// let value = serde_json::to_vec(&json!({
    ///     "some": "data",
    /// }))?;
    ///
    /// let encrypted = store_cipher.encrypt_value_data(value.clone())?;
    /// let decrypted = store_cipher.decrypt_value_data(encrypted)?;
    ///
    /// assert_eq!(value, decrypted);
    /// # anyhow::Ok(()) };
    /// ```
    pub fn encrypt_value_data(&self, mut data: Vec<u8>) -> Result<EncryptedValue, Error> {
        let nonce = Keys::get_nonce()?;
        let cipher = XChaCha20Poly1305::new(self.inner.encryption_key());

        let ciphertext = cipher.encrypt(XNonce::from_slice(&nonce), data.as_ref())?;

        data.zeroize();
        Ok(EncryptedValue { version: VERSION, ciphertext, nonce })
    }

    /// Decrypt a value after it was fetchetd from the key/value store.
    ///
    /// A value can be encrypted using the [`StoreCipher::encrypt_value()`]
    /// method.
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
    /// use matrix_sdk_store_encryption::StoreCipher;
    /// use serde_json::{json, value::Value};
    ///
    /// let store_cipher = StoreCipher::new()?;
    ///
    /// let value = json!({
    ///     "some": "data",
    /// });
    ///
    /// let encrypted = store_cipher.encrypt_value(&value)?;
    /// let decrypted: Value = store_cipher.decrypt_value(&encrypted)?;
    ///
    /// assert_eq!(value, decrypted);
    /// # anyhow::Ok(()) };
    /// ```
    pub fn decrypt_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T, Error> {
        let value: EncryptedValue = serde_json::from_slice(value)?;
        self.decrypt_value_typed(value)
    }

    /// Decrypt a value after it was fetchetd from the key/value store.
    ///
    /// A value can be encrypted using the
    /// [`StoreCipher::encrypt_value_typed()`] method. Lower level method to
    /// [`StoreCipher::decrypt_value_typed()`]
    ///
    /// # Arguments
    ///
    /// * `value` - The EncryptedValue of a value that should be decrypted.
    ///
    /// The method will deserialize the decrypted value into the expected type.
    ///
    /// # Examples
    ///
    /// ```
    /// # let example = || {
    /// use matrix_sdk_store_encryption::StoreCipher;
    /// use serde_json::{json, value::Value};
    ///
    /// let store_cipher = StoreCipher::new()?;
    ///
    /// let value = json!({
    ///     "some": "data",
    /// });
    ///
    /// let encrypted = store_cipher.encrypt_value_typed(&value)?;
    /// let decrypted: Value = store_cipher.decrypt_value_typed(encrypted)?;
    ///
    /// assert_eq!(value, decrypted);
    /// # anyhow::Ok(()) };
    /// ```
    pub fn decrypt_value_typed<T: DeserializeOwned>(
        &self,
        value: EncryptedValue,
    ) -> Result<T, Error> {
        let mut plaintext = self.decrypt_value_data(value)?;
        let ret = serde_json::from_slice(&plaintext);
        plaintext.zeroize();
        Ok(ret?)
    }

    /// Decrypt a value after it was fetchetd from the key/value store.
    ///
    /// A value can be encrypted using the [`StoreCipher::encrypt_value_data()`]
    /// method. Lower level method to [`StoreCipher::decrypt_value()`].
    ///
    /// # Arguments
    ///
    /// * `value` - The EncryptedValue of a value that should be decrypted.
    ///
    /// The method will return the raw decrypted value
    ///
    /// # Examples
    ///
    /// ```
    /// # let example = || {
    /// use matrix_sdk_store_encryption::StoreCipher;
    /// use serde_json::{json, value::Value};
    ///
    /// let store_cipher = StoreCipher::new()?;
    ///
    /// let value = serde_json::to_vec(&json!({
    ///     "some": "data",
    /// }))?;
    ///
    /// let encrypted = store_cipher.encrypt_value_data(value.clone())?;
    /// let decrypted = store_cipher.decrypt_value_data(encrypted)?;
    ///
    /// assert_eq!(value, decrypted);
    /// # anyhow::Ok(()) };
    /// ```
    pub fn decrypt_value_data(&self, value: EncryptedValue) -> Result<Vec<u8>, Error> {
        if value.version != VERSION {
            return Err(Error::Version(VERSION, value.version));
        }

        let cipher = XChaCha20Poly1305::new(self.inner.encryption_key());
        let nonce = XNonce::from_slice(&value.nonce);
        Ok(cipher.decrypt(nonce, value.ciphertext.as_ref())?)
    }

    /// Expand the given passphrase into a KEY_SIZE long key.
    fn expand_key(passphrase: &str, salt: &[u8], rounds: u32) -> Box<[u8; 32]> {
        let mut key = Box::new([0u8; 32]);
        pbkdf2::<Hmac<Sha256>>(passphrase.as_bytes(), salt, rounds, key.deref_mut()).expect(
            "We should be able to expand a passphrase of any length due to \
             HMAC being able to be initialized with any input size",
        );

        key
    }
}

#[derive(Zeroize)]
#[zeroize(drop)]
struct MacKey(Box<[u8; 32]>);

impl MacKey {
    fn mac(&self, input: &[u8]) -> Hash {
        blake3::keyed_hash(&self.0, input)
    }
}

/// Encrypted value, ready for storage, as created by the
/// [`StoreCipher::encrypt_value_data()`]
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncryptedValue {
    version: u8,
    ciphertext: Vec<u8>,
    nonce: [u8; XNONCE_SIZE],
}

#[derive(Zeroize)]
#[zeroize(drop)]
struct Keys {
    encryption_key: Box<[u8; 32]>,
    mac_key_seed: Box<MacKeySeed>,
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
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
enum KdfInfo {
    None,
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
/// store cipher.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
enum CipherTextInfo {
    /// A store cipher encrypted using the ChaCha20Poly1305 AEAD.
    ChaCha20Poly1305 {
        /// The nonce that was used to encrypt the ciphertext.
        nonce: [u8; XNONCE_SIZE],
        /// The encrypted store cipher.
        ciphertext: Vec<u8>,
    },
}

/// An encrypted version of our store cipher, this can be safely stored in a
/// database.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct EncryptedStoreCipher {
    /// Info about the key derivation method that was used to expand the
    /// passphrase into an encryption key.
    pub kdf_info: KdfInfo,
    /// The ciphertext with it's accompanying additional data that is needed to
    /// decrypt the store cipher.
    pub ciphertext_info: CipherTextInfo,
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::{Error, StoreCipher};

    #[test]
    fn generating() {
        StoreCipher::new().unwrap();
    }

    #[test]
    fn exporting_store_cipher() -> Result<(), Error> {
        let passphrase = "it's a secret to everybody";
        let store_cipher = StoreCipher::new()?;

        let value = json!({
            "some": "data"
        });

        let encrypted_value = store_cipher.encrypt_value(&value)?;

        let encrypted = store_cipher._insecure_export_fast_for_testing(passphrase)?;
        let decrypted = StoreCipher::import(passphrase, &encrypted)?;

        assert_eq!(store_cipher.inner.encryption_key, decrypted.inner.encryption_key);
        assert_eq!(store_cipher.inner.mac_key_seed, decrypted.inner.mac_key_seed);

        let decrypted_value: Value = decrypted.decrypt_value(&encrypted_value)?;

        assert_eq!(value, decrypted_value);

        // Can't use assert matches here since we don't have a Debug implementation for
        // StoreCipher.
        match StoreCipher::import_with_key(&[0u8; 32], &encrypted) {
            Err(Error::KdfMismatch) => {}
            _ => panic!(
                "Invalid error when importing a passphrase-encrypted store cipher with a key"
            ),
        }

        let store_cipher = StoreCipher::new()?;
        let encrypted_value = store_cipher.encrypt_value(&value)?;

        let export = store_cipher.export_with_key(&[0u8; 32])?;
        let decrypted = StoreCipher::import_with_key(&[0u8; 32], &export)?;

        let decrypted_value: Value = decrypted.decrypt_value(&encrypted_value)?;
        assert_eq!(value, decrypted_value);

        // Same as above, can't use assert_matches.
        match StoreCipher::import_with_key(&[0u8; 32], &encrypted) {
            Err(Error::KdfMismatch) => {}
            _ => panic!(
                "Invalid error when importing a key-encrypted store cipher with a passphrase"
            ),
        }

        let old_export = json!({
            "ciphertext_info": {
                "ChaCha20Poly1305":{
                    "ciphertext":[
                        136,202,212,194,9,223,171,109,152,84,140,183,14,55,198,22,150,130,80,135,
                        161,202,79,205,151,202,120,91,108,154,252,94,56,178,108,216,186,179,167,128,
                        154,107,243,195,14,138,86,78,140,159,245,170,204,227,27,84,255,161,196,69,
                        60,150,69,123,67,134,28,50,10,179,250,141,221,19,202,132,28,122,92,116
                    ],
                    "nonce":[
                        108,3,115,54,65,135,250,188,212,204,93,223,78,11,52,46,
                        124,140,218,73,88,167,50,230
                    ]
                }
            },
            "kdf_info":{
                "Pbkdf2ToChaCha20Poly1305":{
                    "kdf_salt":[
                        221,133,149,116,199,122,172,189,236,42,26,204,53,164,245,158,137,113,
                        31,220,239,66,64,51,242,164,185,166,176,218,209,245
                    ],
                    "rounds":1000
                }
            }
        });

        let old_export = serde_json::to_vec(&old_export)?;

        StoreCipher::import(passphrase, &old_export)
            .expect("We can import the old store-cipher export");

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

        let store_cipher = StoreCipher::new()?;

        let encrypted = store_cipher.encrypt_value(&event)?;
        let decrypted: Value = store_cipher.decrypt_value(&encrypted)?;

        assert_eq!(event, decrypted);

        Ok(())
    }

    #[test]
    fn encrypting_keys() -> Result<(), Error> {
        let store_cipher = StoreCipher::new()?;

        let first = store_cipher.hash_key("some_table", b"It's dangerous to go alone");
        let second = store_cipher.hash_key("some_table", b"It's dangerous to go alone");
        let third = store_cipher.hash_key("another_table", b"It's dangerous to go alone");
        let fourth = store_cipher.hash_key("another_table", b"It's dangerous to go alone");
        let fifth = store_cipher.hash_key("another_table", b"It's not dangerous to go alone");

        assert_eq!(first, second);
        assert_ne!(first, third);
        assert_eq!(third, fourth);
        assert_ne!(fourth, fifth);

        Ok(())
    }
}
