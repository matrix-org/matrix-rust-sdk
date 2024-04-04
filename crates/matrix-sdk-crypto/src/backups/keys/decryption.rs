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
    io::{Cursor, Read},
    ops::DerefMut,
};

use ruma::api::client::backup::EncryptedSessionData;
use thiserror::Error;
use vodozemac::Curve25519PublicKey;
use zeroize::{Zeroize, Zeroizing};

use super::{
    compat::{Error as DecryptionError, Message, PkDecryption},
    MegolmV1BackupKey,
};
use crate::{
    olm::BackedUpRoomKey,
    store::BackupDecryptionKey,
    types::{MegolmV1AuthData, RoomKeyBackupInfo},
};

/// Error type for the decoding of a [`BackupDecryptionKey`].
#[derive(Debug, Error)]
pub enum DecodeError {
    /// The decoded recovery key has an invalid prefix.
    #[error("The decoded recovery key has an invalid prefix: expected {0:?}, got {1:?}")]
    Prefix([u8; 2], [u8; 2]),
    /// The parity byte of the recovery key didn't match.
    #[error("The parity byte of the recovery key doesn't match: expected {0:?}, got {1:?}")]
    Parity(u8, u8),
    /// The recovery key has an invalid length.
    #[error("The decoded recovery key has a invalid length: expected {0}, got {1}")]
    Length(usize, usize),
    /// The recovery key isn't valid base58.
    #[error(transparent)]
    Base58(#[from] bs58::decode::Error),
    /// The  recovery key isn't valid base64.
    #[error(transparent)]
    Base64(#[from] vodozemac::Base64DecodeError),
    /// The recovery key is too short, we couldn't read enough data.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The recovery key, a Curve25519 public key, couldn't be decoded.
    #[error(transparent)]
    PublicKey(#[from] vodozemac::KeyError),
}

impl TryFrom<String> for BackupDecryptionKey {
    type Error = DecodeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_base58(&value)
    }
}

impl std::fmt::Display for BackupDecryptionKey {
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

impl BackupDecryptionKey {
    const PREFIX: [u8; 2] = [0x8b, 0x01];
    const PREFIX_PARITY: u8 = Self::PREFIX[0] ^ Self::PREFIX[1];
    const DISPLAY_CHUNK_SIZE: usize = 4;

    fn parity_byte(bytes: &[u8]) -> u8 {
        bytes.iter().fold(Self::PREFIX_PARITY, |acc, x| acc ^ x)
    }

    /// Create a new decryption key from the given byte array.
    ///
    /// **Warning**: You need to make sure that the byte array contains correct
    /// random data, either by using a random number generator or by using an
    /// exported version of a previously created [`BackupDecryptionKey`].
    pub fn from_bytes(key: &[u8; Self::KEY_SIZE]) -> Self {
        let mut inner = Box::new([0u8; Self::KEY_SIZE]);
        inner.copy_from_slice(key);

        Self::from_boxed_bytes(inner)
    }

    fn from_boxed_bytes(key: Box<[u8; Self::KEY_SIZE]>) -> Self {
        Self { inner: key }
    }

    /// Get the decryption key as a raw byte representation.
    pub fn as_bytes(&self) -> &[u8; Self::KEY_SIZE] {
        &self.inner
    }

    /// Try to create a [`BackupDecryptionKey`] from a base64 export.
    pub fn from_base64(key: &str) -> Result<Self, DecodeError> {
        let decoded = Zeroizing::new(vodozemac::base64_decode(key)?);

        if decoded.len() != Self::KEY_SIZE {
            Err(DecodeError::Length(Self::KEY_SIZE, decoded.len()))
        } else {
            let mut key = Box::new([0u8; Self::KEY_SIZE]);
            key.copy_from_slice(&decoded);

            Ok(Self::from_boxed_bytes(key))
        }
    }

    /// Try to create a [`BackupDecryptionKey`] from a base58 export.
    pub fn from_base58(value: &str) -> Result<Self, DecodeError> {
        // Remove any whitespace we might have
        let value: String = value.chars().filter(|c| !c.is_whitespace()).collect();

        let decoded = bs58::decode(value).with_alphabet(bs58::Alphabet::BITCOIN).into_vec()?;
        let mut decoded = Cursor::new(decoded);

        let mut prefix = [0u8; 2];
        let mut key = Box::new([0u8; Self::KEY_SIZE]);
        let mut expected_parity = [0u8; 1];

        decoded.read_exact(&mut prefix)?;
        decoded.read_exact(key.deref_mut())?;
        decoded.read_exact(&mut expected_parity)?;

        let expected_parity = expected_parity[0];
        let parity = Self::parity_byte(key.as_ref());

        let _ = Zeroizing::new(decoded.into_inner());

        if prefix != Self::PREFIX {
            Err(DecodeError::Prefix(Self::PREFIX, prefix))
        } else if expected_parity != parity {
            Err(DecodeError::Parity(expected_parity, parity))
        } else {
            Ok(Self::from_boxed_bytes(key))
        }
    }

    /// Export the `[`BackupDecryptionKey`] as a base58 encoded string.
    pub fn to_base58(&self) -> String {
        let bytes = Zeroizing::new(
            [
                Self::PREFIX.as_ref(),
                self.inner.as_ref(),
                [Self::parity_byte(self.inner.as_ref())].as_ref(),
            ]
            .concat(),
        );

        bs58::encode(bytes.as_slice()).with_alphabet(bs58::Alphabet::BITCOIN).into_string()
    }

    fn get_pk_decryption(&self) -> PkDecryption {
        PkDecryption::from_bytes(self.inner.as_ref())
    }

    /// Extract the megolm.v1 public key from this [`BackupDecryptionKey`].
    pub fn megolm_v1_public_key(&self) -> MegolmV1BackupKey {
        let pk = self.get_pk_decryption();
        MegolmV1BackupKey::new(pk.public_key(), None)
    }

    /// Get the [`RoomKeyBackupInfo`] for this [`BackupDecryptionKey`].
    ///
    /// The [`RoomKeyBackupInfo`] can be uploaded to the homeserver to activate
    /// a new backup version.
    pub fn to_backup_info(&self) -> RoomKeyBackupInfo {
        let pk = self.get_pk_decryption();
        let auth_data = MegolmV1AuthData::new(pk.public_key(), Default::default());

        RoomKeyBackupInfo::MegolmBackupV1Curve25519AesSha2(auth_data)
    }

    /// Try to decrypt the given ciphertext using this [`BackupDecryptionKey`].
    ///
    /// This will use the [`m.megolm_backup.v1.curve25519-aes-sha2`] algorithm
    /// to decrypt the given ciphertext.
    ///
    /// [`m.megolm_backup.v1.curve25519-aes-sha2`]:
    /// https://spec.matrix.org/unstable/client-server-api/#backup-algorithm-mmegolm_backupv1curve25519-aes-sha2
    pub fn decrypt_v1(
        &self,
        ephemeral_key: &str,
        mac: &str,
        ciphertext: &str,
    ) -> Result<String, DecryptionError> {
        let message = Message::from_base64(ciphertext, mac, ephemeral_key)?;
        let pk = self.get_pk_decryption();

        let decrypted = pk.decrypt(&message)?;

        Ok(String::from_utf8_lossy(&decrypted).to_string())
    }

    /// Try to decrypt the given [`EncryptedSessionData`] using this
    /// [`BackupDecryptionKey`].
    pub fn decrypt_session_data(
        &self,
        session_data: EncryptedSessionData,
    ) -> Result<BackedUpRoomKey, DecryptionError> {
        let message = Message {
            ciphertext: session_data.ciphertext.into_inner(),
            mac: session_data.mac.into_inner(),
            ephemeral_key: Curve25519PublicKey::from_slice(session_data.ephemeral.as_bytes())?,
        };

        let pk = self.get_pk_decryption();

        let mut decrypted = pk.decrypt(&message)?;
        let result = serde_json::from_slice(&decrypted);

        decrypted.zeroize();

        Ok(result?)
    }

    /// Check if the given public key from the [`RoomKeyBackupInfo`] matches to
    /// this [`BackupDecryptionKey`].
    pub fn backup_key_matches(&self, info: &RoomKeyBackupInfo) -> bool {
        match info {
            RoomKeyBackupInfo::MegolmBackupV1Curve25519AesSha2(info) => {
                let pk = self.get_pk_decryption();
                let public_key = pk.public_key();

                info.public_key == public_key
            }
            RoomKeyBackupInfo::Other { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;
    use ruma::api::client::backup::KeyBackupData;
    use serde_json::json;

    use super::{BackupDecryptionKey, DecodeError};
    use crate::olm::{BackedUpRoomKey, ExportedRoomKey, InboundGroupSession};

    const TEST_KEY: [u8; 32] = [
        0x77, 0x07, 0x6D, 0x0A, 0x73, 0x18, 0xA5, 0x7D, 0x3C, 0x16, 0xC1, 0x72, 0x51, 0xB2, 0x66,
        0x45, 0xDF, 0x4C, 0x2F, 0x87, 0xEB, 0xC0, 0x99, 0x2A, 0xB1, 0x77, 0xFB, 0xA5, 0x1D, 0xB9,
        0x2C, 0x2A,
    ];

    fn room_key() -> ExportedRoomKey {
        let json = json!({
            "algorithm": "m.megolm.v1.aes-sha2",
            "sender_key": "DeHIg4gwhClxzFYcmNntPNF9YtsdZbmMy8+3kzCMXHA",
            "session_id": "gM8i47Xhu0q52xLfgUXzanCMpLinoyVyH7R58cBuVBU",
            "room_id": "!DovneieKSTkdHKpIXy:morpheus.localhost",
            "session_key": "AQAAAABvWMNZjKFtebYIePKieQguozuoLgzeY6wKcyJjLJcJtQgy1dPqTBD12U+XrYLrRHn\
                            lKmxoozlhFqJl456+9hlHCL+yq+6ScFuBHtJepnY1l2bdLb4T0JMDkNsNErkiLiLnD6yp3J\
                            DSjIhkdHxmup/huygrmroq6/L5TaThEoqvW4DPIuO14btKudsS34FF82pwjKS4p6Mlch+0e\
                            fHAblQV",
            "sender_claimed_keys":{},
            "forwarding_curve25519_key_chain":[]
        });

        serde_json::from_value(json)
            .expect("We should be able to deserialize our backed up room key")
    }

    #[test]
    fn base64_decoding() -> Result<(), DecodeError> {
        let key = BackupDecryptionKey::new().expect("Can't create a new recovery key");

        let base64 = key.to_base64();
        let decoded_key = BackupDecryptionKey::from_base64(&base64)?;
        assert_eq!(key.inner, decoded_key.inner, "The decode key doesn't match the original");

        BackupDecryptionKey::from_base64("i").expect_err("The recovery key is too short");

        Ok(())
    }

    #[test]
    fn base58_decoding() -> Result<(), DecodeError> {
        let key = BackupDecryptionKey::new().expect("Can't create a new recovery key");

        let base64 = key.to_base58();
        let decoded_key = BackupDecryptionKey::from_base58(&base64)?;
        assert_eq!(key.inner, decoded_key.inner, "The decode key doesn't match the original");

        let test_key =
            BackupDecryptionKey::from_base58("EsTcLW2KPGiFwKEA3As5g5c4BXwkqeeJZJV8Q9fugUMNUE4d")?;
        assert_eq!(
            test_key.as_bytes(),
            &TEST_KEY,
            "The decoded recovery key doesn't match the test key"
        );

        let test_key = BackupDecryptionKey::from_base58(
            "EsTc LW2K PGiF wKEA 3As5 g5c4 BXwk qeeJ ZJV8 Q9fu gUMN UE4d",
        )?;
        assert_eq!(
            test_key.as_bytes(),
            &TEST_KEY,
            "The decoded recovery key doesn't match the test key"
        );

        BackupDecryptionKey::from_base58(
            "EsTc LW2K PGiF wKEA 3As5 g5c4 BXwk qeeJ ZJV8 Q9fu gUMN UE4e",
        )
        .expect_err("Can't create a recovery key if the parity byte is invalid");

        Ok(())
    }

    #[test]
    fn test_decrypt_key() {
        let decryption_key =
            BackupDecryptionKey::from_base64("Ha9cklU/9NqFo9WKdVfGzmqUL/9wlkdxfEitbSIPVXw")
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

        let decrypted = decryption_key
            .decrypt_v1(&ephemeral, &mac, &ciphertext)
            .expect("The backed up key should be decrypted successfully");

        let _: BackedUpRoomKey = serde_json::from_str(&decrypted)
            .expect("The decrypted payload should contain valid JSON");

        let _ = decryption_key
            .decrypt_session_data(key_backup_data.session_data)
            .expect("The backed up key should be decrypted successfully");
    }

    #[async_test]
    async fn test_encryption_cycle() {
        let session = InboundGroupSession::from_export(&room_key()).unwrap();

        let decryption_key = BackupDecryptionKey::new().unwrap();
        let encryption_key = decryption_key.megolm_v1_public_key();

        let encrypted = encryption_key.encrypt(session).await;

        let _ = decryption_key
            .decrypt_session_data(encrypted.session_data)
            .expect("We should be able to decrypt a just encrypted room key");
    }

    #[test]
    fn key_matches() {
        let decryption_key = BackupDecryptionKey::new().unwrap();

        let key_info = decryption_key.to_backup_info();

        assert!(
            decryption_key.backup_key_matches(&key_info),
            "The backup info should match the decryption key"
        );
    }
}
