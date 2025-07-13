use std::{collections::HashMap, iter, ops::DerefMut, sync::Arc};

use hmac::Hmac;
use matrix_sdk_crypto::{
    backups::DecryptionError,
    store::{types::BackupDecryptionKey, CryptoStoreError as InnerStoreError},
};
use pbkdf2::pbkdf2;
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use sha2::Sha512;
use thiserror::Error;
use zeroize::Zeroize;

/// The private part of the backup key, the one used for recovery.
#[derive(uniffi::Object)]
pub struct BackupRecoveryKey {
    pub(crate) inner: BackupDecryptionKey,
    pub(crate) passphrase_info: Option<PassphraseInfo>,
}

/// Error type for the decryption of backed up room keys.
#[derive(Debug, Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum PkDecryptionError {
    /// An internal libolm error happened during decryption.
    #[error("Error decryption a PkMessage {0}")]
    Olm(#[from] DecryptionError),
}

/// Error type for the decoding and storing of the backup key.
#[derive(Debug, Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum DecodeError {
    /// An error happened while decoding the recovery key.
    #[error(transparent)]
    Decode(#[from] matrix_sdk_crypto::backups::DecodeError),
    /// An error happened in the storage layer while trying to save the
    /// decoded recovery key.
    #[error(transparent)]
    CryptoStore(#[from] InnerStoreError),
}

/// Struct containing info about the way the backup key got derived from a
/// passphrase.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PassphraseInfo {
    /// The salt that was used during key derivation.
    pub private_key_salt: String,
    /// The number of PBKDF rounds that were used for key derivation.
    pub private_key_iterations: i32,
}

/// The public part of the backup key.
#[derive(uniffi::Record)]
pub struct MegolmV1BackupKey {
    /// The actual base64 encoded public key.
    pub public_key: String,
    /// Signatures that have signed our backup key.
    pub signatures: HashMap<String, HashMap<String, String>>,
    /// The passphrase info, if the key was derived from one.
    pub passphrase_info: Option<PassphraseInfo>,
    /// Get the full name of the backup algorithm this backup key supports.
    pub backup_algorithm: String,
}

impl BackupRecoveryKey {
    const KEY_SIZE: usize = 32;
    const SALT_SIZE: usize = 32;
    const PBKDF_ROUNDS: i32 = 500_000;
}

#[matrix_sdk_ffi_macros::export]
impl BackupRecoveryKey {
    /// Create a new random [`BackupRecoveryKey`].
    #[allow(clippy::new_without_default)]
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: BackupDecryptionKey::new()
                .expect("Can't gather enough randomness to create a recovery key"),
            passphrase_info: None,
        })
    }

    /// Try to create a [`BackupRecoveryKey`] from a base 64 encoded string.
    #[uniffi::constructor]
    pub fn from_base64(key: String) -> Result<Arc<Self>, DecodeError> {
        Ok(Arc::new(Self { inner: BackupDecryptionKey::from_base64(&key)?, passphrase_info: None }))
    }

    /// Try to create a [`BackupRecoveryKey`] from a base 58 encoded string.
    #[uniffi::constructor]
    pub fn from_base58(key: String) -> Result<Arc<Self>, DecodeError> {
        Ok(Arc::new(Self { inner: BackupDecryptionKey::from_base58(&key)?, passphrase_info: None }))
    }

    /// Create a new [`BackupRecoveryKey`] from the given passphrase.
    #[uniffi::constructor]
    pub fn new_from_passphrase(passphrase: String) -> Arc<Self> {
        let mut rng = thread_rng();
        let salt: String = iter::repeat(())
            .map(|()| rng.sample(Alphanumeric))
            .map(char::from)
            .take(Self::SALT_SIZE)
            .collect();

        Self::from_passphrase(passphrase, salt, Self::PBKDF_ROUNDS)
    }

    /// Restore a [`BackupRecoveryKey`] from the given passphrase.
    #[uniffi::constructor]
    pub fn from_passphrase(passphrase: String, salt: String, rounds: i32) -> Arc<Self> {
        let mut key = Box::new([0u8; Self::KEY_SIZE]);
        let rounds = rounds as u32;

        pbkdf2::<Hmac<Sha512>>(passphrase.as_bytes(), salt.as_bytes(), rounds, key.deref_mut())
            .expect(
                "We should be able to expand a passphrase of any length due to \
                 HMAC being able to be initialized with any input size",
            );

        let backup_decryption_key = BackupDecryptionKey::from_bytes(&key);

        key.zeroize();

        Arc::new(Self {
            inner: backup_decryption_key,
            passphrase_info: Some(PassphraseInfo {
                private_key_salt: salt,
                private_key_iterations: rounds as i32,
            }),
        })
    }

    /// Convert the recovery key to a base 58 encoded string.
    pub fn to_base58(&self) -> String {
        self.inner.to_base58()
    }

    /// Convert the recovery key to a base 64 encoded string.
    pub fn to_base64(&self) -> String {
        self.inner.to_base64()
    }

    /// Get the public part of the backup key.
    pub fn megolm_v1_public_key(&self) -> MegolmV1BackupKey {
        let public_key = self.inner.megolm_v1_public_key();

        let signatures: HashMap<String, HashMap<String, String>> = public_key
            .signatures()
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    v.into_iter()
                        .map(|(k, v)| {
                            (
                                k.to_string(),
                                match v {
                                    Ok(s) => s.to_base64(),
                                    Err(s) => s.source,
                                },
                            )
                        })
                        .collect(),
                )
            })
            .collect();

        MegolmV1BackupKey {
            public_key: public_key.to_base64(),
            signatures,
            passphrase_info: self.passphrase_info.clone(),
            backup_algorithm: public_key.backup_algorithm().to_owned(),
        }
    }

    /// Try to decrypt a message that was encrypted using the public part of the
    /// backup key.
    pub fn decrypt_v1(
        &self,
        ephemeral_key: String,
        mac: String,
        ciphertext: String,
    ) -> Result<String, PkDecryptionError> {
        self.inner.decrypt_v1(&ephemeral_key, &mac, &ciphertext).map_err(|e| e.into())
    }
}

#[cfg(test)]
mod tests {
    use ruma::api::client::backup::KeyBackupData;
    use serde_json::json;

    use super::BackupRecoveryKey;

    #[test]
    fn test_decrypt_key() {
        let recovery_key = BackupRecoveryKey::from_base64(
            "Ha9cklU/9NqFo9WKdVfGzmqUL/9wlkdxfEitbSIPVXw".to_owned(),
        )
        .unwrap();

        let data = json!({
            "first_message_index": 0,
            "forwarded_count": 0,
            "is_verified": false,
            "session_data": {
                "ephemeral": "HlLi76oV6wxHz3PCqE/bxJi6yF1HnYz5Dq3T+d/KpRw",
                "ciphertext": "MuM8E3Yc6TSAvhVGb77rQ++jE6p9dRepx63/3YPD2wACKAppkZHeFrnTH6wJ/HSyrmzo\
                               7HfwqVl6tKNpfooSTHqUf6x1LHz+h4B/Id5ITO1WYt16AaI40LOnZqTkJZCfSPuE2oxa\
                               lwEHnCS3biWybutcnrBFPR3LMtaeHvvkb+k3ny9l5ZpsU9G7vCm3XoeYkWfLekWXvDhb\
                               qWrylXD0+CNUuaQJ/S527TzLd4XKctqVjjO/cCH7q+9utt9WJAfK8LGaWT/mZ3AeWjf5\
                               kiqOpKKf5Cn4n5SSil5p/pvGYmjnURvZSEeQIzHgvunIBEPtzK/MYEPOXe/P5achNGlC\
                               x+5N19Ftyp9TFaTFlTWCTi0mpD7ePfCNISrwpozAz9HZc0OhA8+1aSc7rhYFIeAYXFU3\
                               26NuFIFHI5pvpSxjzPQlOA+mavIKmiRAtjlLw11IVKTxgrdT4N8lXeMr4ndCSmvIkAzF\
                               Mo1uZA4fzjiAdQJE4/2WeXFNNpvdfoYmX8Zl9CAYjpSO5HvpwkAbk4/iLEH3hDfCVUwD\
                               fMh05PdGLnxeRpiEFWSMSsJNp+OWAA+5JsF41BoRGrxoXXT+VKqlUDONd+O296Psu8Q+\
                               d8/S618",
                "mac": "GtMrurhDTwo"
            }
        });

        let key_backup_data: KeyBackupData = serde_json::from_value(data).unwrap();
        let ephemeral = key_backup_data.session_data.ephemeral.encode();
        let ciphertext = key_backup_data.session_data.ciphertext.encode();
        let mac = key_backup_data.session_data.mac.encode();

        let _ = recovery_key
            .decrypt_v1(ephemeral, mac, ciphertext)
            .expect("The backed up key should be decrypted successfully");
    }
}
