//! Megolm backup types

use std::{collections::HashMap, iter, ops::DerefMut};

use js_sys::{JsString, JSON};
use wasm_bindgen::prelude::*;

use hmac::Hmac;
use matrix_sdk_crypto::{backups::MegolmV1BackupKey as InnerMegolmV1BackupKey, store::RecoveryKey};

use pbkdf2::pbkdf2;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use serde_wasm_bindgen;
use sha2::Sha512;
use zeroize::Zeroize;

/// The private part of the backup key, the one used for recovery.
#[derive(Debug)]
#[wasm_bindgen]
pub struct BackupRecoveryKey {
    pub(crate) inner: RecoveryKey,
    pub(crate) passphrase_info: Option<PassphraseInfo>,
}

/// Struct containing info about the way the backup key got derived from a
/// passphrase.
#[derive(Debug, Clone)]
#[wasm_bindgen]
pub struct PassphraseInfo {
    /// The salt that was used during key derivation.
    #[wasm_bindgen(getter_with_clone)]
    pub private_key_salt: JsString,
    /// The number of PBKDF rounds that were used for key derivation.
    pub private_key_iterations: i32,
}

/// The public part of the backup key.
#[derive(Debug, Clone)]
#[wasm_bindgen]
pub struct MegolmV1BackupKey {
    inner: InnerMegolmV1BackupKey,
    passphrase_info: Option<PassphraseInfo>,
}

#[wasm_bindgen]
impl MegolmV1BackupKey {
    /// The actual base64 encoded public key.
    #[wasm_bindgen(getter, js_name = "publicKeyBase64")]
    pub fn public_key(&self) -> JsString {
        self.inner.to_base64().into()
    }

    /// The passphrase info, if the key was derived from one.
    #[wasm_bindgen(getter, js_name = "passphraseInfo")]
    pub fn passphrase_info(&self) -> Option<PassphraseInfo> {
        self.passphrase_info.clone()
    }

    /// Get the full name of the backup algorithm this backup key supports.
    #[wasm_bindgen(getter, js_name = "algorithm")]
    pub fn backup_algorithm(&self) -> JsString {
        self.inner.backup_algorithm().into()
    }

    /// Signatures that have signed our backup key.
    /// map of userId to map of deviceOrKeyId to signature
    #[wasm_bindgen(getter, js_name = "signatures")]
    pub fn signatures(&self) -> JsValue {
        let signatures: HashMap<String, HashMap<String, String>> = self
            .inner
            .signatures()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.into_iter().map(|(k, v)| (k.to_string(), v)).collect()))
            .collect();

        serde_wasm_bindgen::to_value(&signatures).unwrap()
    }
}

impl BackupRecoveryKey {
    const KEY_SIZE: usize = 32;
    const SALT_SIZE: usize = 32;
    const PBKDF_ROUNDS: i32 = 500_000;
}

#[wasm_bindgen]
impl BackupRecoveryKey {
    /// Create a new random [`BackupRecoveryKey`].
    #[wasm_bindgen(js_name = "createRandomKey")]
    pub fn create_random_key() -> BackupRecoveryKey {
        BackupRecoveryKey {
            inner: RecoveryKey::new()
                .expect("Can't gather enough randomness to create a recovery key"),
            passphrase_info: None,
        }
    }

    /// Try to create a [`BackupRecoveryKey`] from a base 64 encoded string.
    #[wasm_bindgen(js_name = "fromBase64")]
    pub fn from_base64(key: String) -> Result<BackupRecoveryKey, JsError> {
        Ok(Self { inner: RecoveryKey::from_base64(&key)?, passphrase_info: None })
    }

    /// Try to create a [`BackupRecoveryKey`] from a base 58 encoded string.
    #[wasm_bindgen(js_name = "fromBase58")]
    pub fn from_base58(key: String) -> Result<BackupRecoveryKey, JsError> {
        Ok(Self { inner: RecoveryKey::from_base58(&key)?, passphrase_info: None })
    }

    /// Create a new [`BackupRecoveryKey`] from the given passphrase.
    #[wasm_bindgen(js_name = "newFromPassphrase")]
    pub fn new_from_passphrase(passphrase: String) -> BackupRecoveryKey {
        let mut rng = thread_rng();
        let salt: String = iter::repeat(())
            .map(|()| rng.sample(Alphanumeric))
            .map(char::from)
            .take(Self::SALT_SIZE)
            .collect();

        BackupRecoveryKey::from_passphrase(passphrase, salt, Self::PBKDF_ROUNDS)
    }

    /// Restore a [`BackupRecoveryKey`] from the given passphrase.
    #[wasm_bindgen(js_name = "fromPassphrase")]
    pub fn from_passphrase(passphrase: String, salt: String, rounds: i32) -> Self {
        let mut key = Box::new([0u8; Self::KEY_SIZE]);
        let rounds = rounds as u32;

        pbkdf2::<Hmac<Sha512>>(passphrase.as_bytes(), salt.as_bytes(), rounds, key.deref_mut());

        let recovery_key = RecoveryKey::from_bytes(&key);

        key.zeroize();

        Self {
            inner: recovery_key,
            passphrase_info: Some(PassphraseInfo {
                private_key_salt: salt.into(),
                private_key_iterations: rounds as i32,
            }),
        }
    }

    /// Convert the recovery key to a base 58 encoded string.
    #[wasm_bindgen(js_name = "toBase58")]
    pub fn to_base58(&self) -> JsString {
        self.inner.to_base58().into()
    }

    /// Convert the recovery key to a base 64 encoded string.
    #[wasm_bindgen(js_name = "toBase64")]
    pub fn to_base64(&self) -> JsString {
        self.inner.to_base64().into()
    }

    /// Get the public part of the backup key.
    #[wasm_bindgen(getter, js_name = "megolmV1PublicKey")]
    pub fn megolm_v1_public_key(&self) -> MegolmV1BackupKey {
        let public_key = self.inner.megolm_v1_public_key();

        MegolmV1BackupKey { inner: public_key, passphrase_info: self.passphrase_info.clone() }
    }

    /// Try to decrypt a message that was encrypted using the public part of the
    /// backup key.
    #[wasm_bindgen(js_name = "decryptV1")]
    pub fn decrypt_v1(
        &self,
        ephemeral_key: String,
        mac: String,
        ciphertext: String,
    ) -> Result<JsValue, JsError> {
        self.inner
            .decrypt_v1(&ephemeral_key, &mac, &ciphertext)
            .map_err(|e| e.into())
            .map(|r| JSON::parse(&r).unwrap())
    }
}
