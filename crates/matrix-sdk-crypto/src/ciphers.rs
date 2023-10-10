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

use aes::{
    cipher::{generic_array::GenericArray, IvSizeUser, KeyIvInit, KeySizeUser, StreamCipher},
    Aes256,
};
use ctr::Ctr128BE;
use hmac::{
    digest::{FixedOutput, MacError},
    Hmac, Mac as _,
};
use pbkdf2::pbkdf2;
use rand::{thread_rng, RngCore};
use sha2::{Sha256, Sha512};
use zeroize::{Zeroize, ZeroizeOnDrop};

// We could use the `keysize()` method Aes256Ctr as KeySize exposes, but it's
// not const (yet?), same for the IV size.
pub(crate) const IV_SIZE: usize = 16;
pub(crate) const KEY_SIZE: usize = 32;
pub(crate) const SALT_SIZE: usize = 16;
pub(crate) const MAC_SIZE: usize = 32;

type Aes256Ctr = Ctr128BE<Aes256>;

type Aes256Key = GenericArray<u8, <Aes256Ctr as KeySizeUser>::KeySize>;
type Aes256Iv = GenericArray<u8, <Aes256Ctr as IvSizeUser>::IvSize>;
type HmacSha256Key = [u8; KEY_SIZE];

/// An authentication tag for the HMAC-SHA-256 message authentication algorithm.
#[derive(Debug)]
pub(crate) struct HmacSha256Mac([u8; MAC_SIZE]);

impl HmacSha256Mac {
    /// Represent the MAC tag as an array of bytes.
    pub(crate) fn as_bytes(&self) -> &[u8; MAC_SIZE] {
        &self.0
    }
}

/// Keys used for our combination of AES-CTR-256 and HMAC-SHA-256.
///
/// ⚠️  This struct provides low-level cryptographic primitives.
///
/// This combination is, as of now, used in the following places:
///
/// 1. Secret storage[1]
/// 2. File-based key exports[2]
///
/// [1]: https://spec.matrix.org/v1.8/client-server-api/#msecret_storagev1aes-hmac-sha2
/// [2]: https://spec.matrix.org/v1.8/client-server-api/#key-exports
#[derive(Zeroize, ZeroizeOnDrop)]
pub(crate) struct AesHmacSha2Key {
    aes_key: Box<[u8; KEY_SIZE]>,
    mac_key: Box<[u8; KEY_SIZE]>,
}

impl AesHmacSha2Key {
    /// Create a [`AesHmacSha2Key`] from a passphrase.
    ///
    /// The passphrase will be expanded using the algorithm described in the
    /// "Key export" part of the [spec].
    ///
    /// [spec]: https://spec.matrix.org/v1.8/client-server-api/#key-exports
    pub(crate) fn from_passphrase(
        passphrase: &str,
        pbkdf_rounds: u32,
        salt: &[u8; SALT_SIZE],
    ) -> Self {
        let mut expanded_keys = [0u8; KEY_SIZE * 2];

        pbkdf2::<Hmac<Sha512>>(passphrase.as_bytes(), salt, pbkdf_rounds, &mut expanded_keys)
            .expect(
                "We should be able to expand a passphrase of any length due to \
                 HMAC being able to be initialized with any input size",
            );

        let (aes_key, mac_key) = Self::split_keys(&expanded_keys);

        expanded_keys.zeroize();

        Self { aes_key, mac_key }
    }

    /// Encrypt the given plaintext and return the ciphertext and the
    /// initialization vector.
    ///
    /// ⚠️  This method is a low-level cryptographic primitive.
    ///
    /// The method does not provide authenticity. You *must* call the
    /// [`AesHmacSha2Key::create_mac_tag()`] method after the encryption step to
    /// create a authentication tag.
    pub(crate) fn encrypt(&self, mut plaintext: Vec<u8>) -> (Vec<u8>, [u8; IV_SIZE]) {
        let initialization_vector = Self::generate_iv();
        let mut cipher =
            Aes256Ctr::new(self.aes_key(), Aes256Iv::from_slice(&initialization_vector));
        cipher.apply_keystream(&mut plaintext);

        (plaintext, initialization_vector)
    }

    /// Create an authentication tag for the given ciphertext.
    ///
    /// ⚠️  This method is a low-level cryptographic primitive.
    ///
    /// This method *must* be called after a call to
    /// [`AesHmacSha2Key::encrypt()`]. The authentication tag must be
    /// provided besides the ciphertext for a decryption attempt.
    pub(crate) fn create_mac_tag(&self, ciphertext: &[u8]) -> HmacSha256Mac {
        let mut mac = [0u8; 32];
        let mac_array = GenericArray::from_mut_slice(&mut mac);

        let mut hmac = Hmac::<Sha256>::new_from_slice(self.mac_key())
            .expect("We should be able to create a new HMAC object from our 32 byte MAC key");

        hmac.update(ciphertext);
        hmac.finalize_into(mac_array);

        HmacSha256Mac(mac)
    }

    /// Verify an authentication tag for the given, encrypted, message.
    ///
    /// You *must* use this method to compare the authentication tags. This
    /// method provides a constant-time comparison for the authentication tags.
    ///
    /// This method *must* be called before a call to
    /// [`AesHmacSha2Key::decrypt()`].
    pub(crate) fn verify_mac(&self, message: &[u8], mac: &[u8; MAC_SIZE]) -> Result<(), MacError> {
        let mac_array = GenericArray::from_slice(mac);

        let mut hmac = Hmac::<Sha256>::new_from_slice(self.mac_key())
            .expect("We should be able to create a new HMAC object from our 32 byte MAC key");

        hmac.update(message);
        hmac.verify(mac_array)
    }

    /// Decrypt the given ciphertext and return the decrypted plaintext.
    ///
    /// The method does not provide authenticity. You *must* call the
    /// [`AesHmacSha2Key::verify_mac()`] method before the decryption step to
    /// verify the authentication tag.
    pub(crate) fn decrypt(
        &self,
        mut ciphertext: Vec<u8>,
        initialization_vector: &[u8; IV_SIZE],
    ) -> Vec<u8> {
        let initialization_vector = Aes256Iv::from_slice(initialization_vector.as_slice());
        let mut cipher = Aes256Ctr::new(self.aes_key(), initialization_vector);
        cipher.apply_keystream(&mut ciphertext);

        ciphertext
    }

    fn split_keys(
        expanded_keys: &[u8; KEY_SIZE * 2],
    ) -> (Box<[u8; KEY_SIZE]>, Box<[u8; KEY_SIZE]>) {
        let mut aes_key = Box::new([0u8; KEY_SIZE]);
        let mut mac_key = Box::new([0u8; KEY_SIZE]);

        aes_key.copy_from_slice(&expanded_keys[0..32]);
        mac_key.copy_from_slice(&expanded_keys[32..64]);

        (aes_key, mac_key)
    }

    /// Generate a new, random initialization vector.
    ///
    /// The initialization vector will be clamped and will be used to encrypt
    /// the ciphertext.
    fn generate_iv() -> [u8; IV_SIZE] {
        let mut rng = thread_rng();
        let mut iv = [0u8; IV_SIZE];

        rng.fill_bytes(&mut iv);

        Self::clamp_iv(iv)
    }

    /// The spec tells us to set bit 63 to 0 in some cases for some reason, I'm
    /// not sure why, but fine:
    ///     Generate 16 random bytes, set bit 63 to 0 (in order to work around
    ///     differences in AES-CTR implementations), and use this as the AES
    ///     initialization vector. This becomes the iv property, encoded using
    ///     base64[1].
    ///
    /// [1]: https://spec.matrix.org/v1.8/client-server-api/#msecret_storagev1aes-hmac-sha2
    fn clamp_iv(iv: [u8; 16]) -> [u8; IV_SIZE] {
        let mut iv = u128::from_be_bytes(iv);
        iv &= !(1 << 63);
        iv.to_be_bytes()
    }

    /// Get the encryption key.
    fn aes_key(&self) -> &Aes256Key {
        Aes256Key::from_slice(self.aes_key.as_slice())
    }

    /// Get the authentication key.
    fn mac_key(&self) -> &HmacSha256Key {
        &self.mac_key
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn encryption_roundtrip() {
        let plaintext = "It's a secret to everybody";

        let salt = [0u8; SALT_SIZE];
        let key = AesHmacSha2Key::from_passphrase("My passphrase", 10, &salt);

        let (ciphertext, iv) = key.encrypt(plaintext.as_bytes().to_vec());
        let mac = key.create_mac_tag(&ciphertext);

        key.verify_mac(&ciphertext, mac.as_bytes())
            .expect("The MAC tag should be successfully verified");
        let decrypted = key.decrypt(ciphertext, &iv);

        assert_eq!(
            plaintext.as_bytes(),
            decrypted,
            "An encryption roundtrip should produce the same plaintext"
        );
    }
}
