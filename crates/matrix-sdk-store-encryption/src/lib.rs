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
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroize;

const VERSION: u8 = 2;
const KDF_SALT_SIZE: usize = 32;
const XNONCE_SIZE: usize = 24;
#[cfg(not(test))]
const KDF_ROUNDS: u32 = 200_000;
#[cfg(test)]
const KDF_ROUNDS: u32 = 1000;

type MacKeySeed = [u8; 32];

/// Error type for the `StoreCipher` operations.
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
    /// This method can be used to persist the `StoreCipher` in the key/value
    /// store in a safe manner.
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
        let mut rng = thread_rng();

        let mut salt = [0u8; KDF_SALT_SIZE];
        salt.try_fill(&mut rng)?;

        let key = StoreCipher::expand_key(passphrase, &salt, KDF_ROUNDS);
        let key = ChachaKey::from_slice(key.as_ref());
        let cipher = XChaCha20Poly1305::new(key);

        let nonce = Keys::get_nonce()?;

        let mut keys = [0u8; 64];

        keys[0..32].copy_from_slice(self.inner.encryption_key.as_ref());
        keys[32..64].copy_from_slice(self.inner.mac_key_seed.as_ref());

        let ciphertext = cipher.encrypt(XNonce::from_slice(&nonce), keys.as_ref())?;

        keys.zeroize();

        let store_cipher = EncryptedStoreCipher {
            kdf_info: KdfInfo::Pbkdf2ToChaCha20Poly1305 { rounds: KDF_ROUNDS, kdf_salt: salt },
            ciphertext_info: CipherTextInfo::ChaCha20Poly1305 { nonce, ciphertext },
        };

        Ok(serde_json::to_vec(&store_cipher).expect("Can't serialize the store cipher"))
    }

    /// Restore a store cipher from an encrypted export.
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
        let encrypted: EncryptedStoreCipher = serde_json::from_slice(encrypted)?;

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

    /// Decrypt a value after it was fetched from the key/value store.
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
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct VersionHelper {
            version: u8,
        }

        let version_helper: VersionHelper = serde_json::from_slice(value)?;
        let version = version_helper.version;

        let value = if version == 1 {
            let value: EncryptedValueV1 = serde_json::from_slice(value)?;
            value.into()
        } else if version == 2 {
            serde_json::from_slice(value)?
        } else {
            return Err(Error::Version(version, VERSION));
        };

        self.decrypt_value_typed(value)
    }

    /// Decrypt a value after it was fetched from the key/value store.
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

    /// Decrypt a value after it was fetched from the key/value store.
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
        match value.version {
            1 | 2 => {
                let cipher = XChaCha20Poly1305::new(self.inner.encryption_key());
                let nonce = XNonce::from_slice(&value.nonce);
                Ok(cipher.decrypt(nonce, value.ciphertext.as_ref())?)
            }
            _ => Err(Error::Version(VERSION, value.version)),
        }
    }

    /// Expand the given passphrase into a KEY_SIZE long key.
    fn expand_key(passphrase: &str, salt: &[u8], rounds: u32) -> Box<[u8; 32]> {
        let mut key = Box::new([0u8; 32]);
        pbkdf2::<Hmac<Sha256>>(passphrase.as_bytes(), salt, rounds, &mut *key);

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
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct EncryptedValue {
    version: u8,
    #[serde(with = "base64")]
    ciphertext: Vec<u8>,
    #[serde(with = "base64_array")]
    nonce: [u8; XNONCE_SIZE],
}

/// Encrypted value, ready for storage, as created by the
/// [`StoreCipher::encrypt_value_data()`]
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct EncryptedValueV1 {
    version: u8,
    ciphertext: Vec<u8>,
    nonce: [u8; XNONCE_SIZE],
}

impl From<EncryptedValueV1> for EncryptedValue {
    fn from(v: EncryptedValueV1) -> Self {
        Self { version: v.version, ciphertext: v.ciphertext, nonce: v.nonce }
    }
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
/// store cipher.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
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
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct EncryptedStoreCipher {
    /// Info about the key derivation method that was used to expand the
    /// passphrase into an encryption key.
    pub kdf_info: KdfInfo,
    /// The ciphertext with it's accompanying additional data that is needed to
    /// decrypt the store cipher.
    pub ciphertext_info: CipherTextInfo,
}

mod base64_array {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::XNONCE_SIZE;

    pub fn serialize<S: Serializer>(v: &[u8; XNONCE_SIZE], s: S) -> Result<S::Ok, S::Error> {
        let base64 = base64::encode(v);
        String::serialize(&base64, s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; XNONCE_SIZE], D::Error> {
        let base64 = String::deserialize(d)?;
        let nonce = base64::decode(base64.as_bytes()).map_err(serde::de::Error::custom)?;

        nonce.try_into().map_err(|_| serde::de::Error::custom("XNonce has an invalid length"))
    }
}

mod base64 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        let base64 = base64::encode(v);
        String::serialize(&base64, s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let base64 = String::deserialize(d)?;
        base64::decode(base64.as_bytes()).map_err(serde::de::Error::custom)
    }
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

        let encrypted = store_cipher.export(passphrase)?;
        let decrypted = StoreCipher::import(passphrase, &encrypted)?;

        assert_eq!(store_cipher.inner.encryption_key, decrypted.inner.encryption_key);
        assert_eq!(store_cipher.inner.mac_key_seed, decrypted.inner.mac_key_seed);

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

    #[test]
    fn decrypt_v1_value() -> Result<(), Error> {
        let passphrase = "it's a secret to everybody";

        let value = [
            123, 34, 118, 101, 114, 115, 105, 111, 110, 34, 58, 49, 44, 34, 99, 105, 112, 104, 101,
            114, 116, 101, 120, 116, 34, 58, 91, 49, 52, 51, 44, 49, 49, 54, 44, 53, 55, 44, 52,
            57, 44, 49, 50, 52, 44, 50, 52, 51, 44, 49, 53, 55, 44, 55, 49, 44, 50, 52, 57, 44, 56,
            44, 57, 56, 44, 49, 50, 49, 44, 52, 55, 44, 52, 50, 44, 50, 48, 48, 44, 50, 54, 44, 49,
            53, 57, 44, 49, 53, 44, 49, 55, 50, 44, 49, 52, 53, 44, 49, 48, 55, 44, 57, 55, 44, 49,
            51, 52, 44, 57, 50, 44, 54, 53, 44, 57, 54, 44, 49, 50, 48, 44, 56, 52, 44, 50, 50, 57,
            44, 49, 55, 49, 44, 49, 57, 93, 44, 34, 110, 111, 110, 99, 101, 34, 58, 91, 49, 48, 49,
            44, 50, 50, 57, 44, 50, 53, 50, 44, 55, 50, 44, 49, 49, 54, 44, 49, 57, 44, 52, 52, 44,
            52, 52, 44, 50, 53, 52, 44, 50, 49, 50, 44, 56, 56, 44, 49, 57, 55, 44, 49, 48, 54, 44,
            50, 51, 57, 44, 50, 50, 48, 44, 50, 48, 50, 44, 49, 48, 50, 44, 50, 50, 49, 44, 49, 49,
            49, 44, 49, 55, 53, 44, 50, 48, 50, 44, 49, 55, 54, 44, 49, 54, 57, 44, 57, 56, 93,
            125,
        ];

        let encrypted_cipher = [
            123, 34, 107, 100, 102, 95, 105, 110, 102, 111, 34, 58, 123, 34, 80, 98, 107, 100, 102,
            50, 84, 111, 67, 104, 97, 67, 104, 97, 50, 48, 80, 111, 108, 121, 49, 51, 48, 53, 34,
            58, 123, 34, 114, 111, 117, 110, 100, 115, 34, 58, 49, 48, 48, 48, 44, 34, 107, 100,
            102, 95, 115, 97, 108, 116, 34, 58, 91, 51, 55, 44, 49, 51, 56, 44, 49, 50, 55, 44, 51,
            49, 44, 49, 49, 55, 44, 53, 49, 44, 50, 52, 57, 44, 49, 51, 49, 44, 49, 52, 44, 50, 49,
            50, 44, 57, 49, 44, 50, 49, 48, 44, 50, 51, 44, 53, 52, 44, 50, 52, 54, 44, 53, 57, 44,
            49, 51, 57, 44, 49, 52, 54, 44, 55, 56, 44, 49, 51, 56, 44, 49, 51, 54, 44, 50, 49, 49,
            44, 49, 57, 49, 44, 49, 55, 53, 44, 49, 57, 57, 44, 50, 44, 57, 49, 44, 50, 53, 49, 44,
            50, 48, 54, 44, 56, 49, 44, 49, 51, 48, 44, 49, 52, 50, 93, 125, 125, 44, 34, 99, 105,
            112, 104, 101, 114, 116, 101, 120, 116, 95, 105, 110, 102, 111, 34, 58, 123, 34, 67,
            104, 97, 67, 104, 97, 50, 48, 80, 111, 108, 121, 49, 51, 48, 53, 34, 58, 123, 34, 110,
            111, 110, 99, 101, 34, 58, 91, 50, 52, 57, 44, 50, 48, 48, 44, 49, 57, 51, 44, 56, 53,
            44, 49, 55, 56, 44, 53, 53, 44, 49, 50, 52, 44, 56, 52, 44, 48, 44, 50, 52, 50, 44, 49,
            52, 50, 44, 53, 57, 44, 49, 50, 53, 44, 55, 51, 44, 49, 57, 48, 44, 49, 55, 55, 44, 49,
            49, 56, 44, 49, 56, 48, 44, 49, 51, 44, 50, 49, 54, 44, 49, 49, 56, 44, 56, 44, 49, 48,
            51, 44, 50, 49, 56, 93, 44, 34, 99, 105, 112, 104, 101, 114, 116, 101, 120, 116, 34,
            58, 91, 50, 51, 51, 44, 52, 52, 44, 50, 51, 56, 44, 50, 48, 52, 44, 53, 44, 56, 49, 44,
            49, 49, 56, 44, 53, 51, 44, 50, 52, 44, 50, 52, 48, 44, 50, 49, 48, 44, 49, 51, 49, 44,
            50, 50, 57, 44, 49, 50, 55, 44, 49, 53, 49, 44, 57, 52, 44, 55, 50, 44, 49, 49, 48, 44,
            49, 51, 44, 49, 57, 44, 57, 56, 44, 49, 54, 57, 44, 50, 53, 50, 44, 49, 55, 53, 44, 49,
            54, 49, 44, 50, 50, 52, 44, 49, 54, 49, 44, 49, 55, 48, 44, 50, 50, 44, 55, 51, 44, 49,
            57, 57, 44, 50, 50, 54, 44, 49, 54, 51, 44, 49, 52, 55, 44, 56, 53, 44, 50, 52, 56, 44,
            49, 48, 57, 44, 49, 48, 57, 44, 50, 49, 50, 44, 49, 52, 52, 44, 50, 48, 49, 44, 53, 48,
            44, 50, 50, 53, 44, 49, 54, 51, 44, 56, 53, 44, 50, 53, 50, 44, 55, 48, 44, 50, 51, 50,
            44, 49, 51, 53, 44, 49, 57, 57, 44, 49, 48, 54, 44, 54, 54, 44, 49, 49, 57, 44, 55, 52,
            44, 50, 48, 52, 44, 52, 51, 44, 49, 55, 49, 44, 49, 53, 54, 44, 56, 55, 44, 49, 54, 53,
            44, 49, 51, 52, 44, 53, 49, 44, 50, 50, 51, 44, 50, 53, 53, 44, 49, 55, 48, 44, 55, 51,
            44, 50, 52, 51, 44, 49, 50, 56, 44, 50, 52, 50, 44, 49, 54, 53, 44, 49, 55, 54, 44, 50,
            51, 55, 44, 55, 49, 44, 52, 52, 44, 52, 57, 44, 57, 57, 44, 49, 54, 50, 44, 49, 56, 52,
            44, 49, 56, 57, 44, 56, 48, 93, 125, 125, 125,
        ];

        let store_cipher = StoreCipher::import(passphrase, &encrypted_cipher)?;

        let plaintext_value = json!({
            "some": "data"
        });

        let decrypted_value: Value = store_cipher.decrypt_value(&value)?;

        assert_eq!(plaintext_value, decrypted_value);

        Ok(())
    }
}
