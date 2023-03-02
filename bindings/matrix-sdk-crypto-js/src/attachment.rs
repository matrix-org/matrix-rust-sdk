//! Attachment API.

use std::io::{Cursor, Read};

use wasm_bindgen::prelude::*;

/// A type to encrypt and to decrypt anything that can fit in an
/// `Uint8Array`, usually big buffer.
#[wasm_bindgen]
#[derive(Debug)]
pub struct Attachment;

#[wasm_bindgen]
impl Attachment {
    /// Encrypt the content of the `Uint8Array`.
    ///
    /// It produces an `EncryptedAttachment`, which can be used to
    /// retrieve the media encryption information, or the encrypted
    /// data.
    #[wasm_bindgen]
    pub fn encrypt(array: &[u8]) -> Result<EncryptedAttachment, JsError> {
        let mut cursor = Cursor::new(array);
        let mut encryptor = matrix_sdk_crypto::AttachmentEncryptor::new(&mut cursor);

        let mut encrypted_data = Vec::new();
        encryptor.read_to_end(&mut encrypted_data)?;

        let media_encryption_info = Some(encryptor.finish());

        Ok(EncryptedAttachment { encrypted_data, media_encryption_info })
    }

    /// Decrypt an `EncryptedAttachment`.
    ///
    /// The encrypted attachment can be created manually, or from the
    /// `encrypt` method.
    ///
    /// **Warning**: The encrypted attachment can be used only
    /// **once**! The encrypted data will still be present, but the
    /// media encryption info (which contain secrets) will be
    /// destroyed. It is still possible to get a JSON-encoded backup
    /// by calling `EncryptedAttachment.mediaEncryptionInfo`.
    pub fn decrypt(attachment: &mut EncryptedAttachment) -> Result<Vec<u8>, JsError> {
        let Some(media_encryption_info) = attachment.media_encryption_info.take() else {
            return Err(JsError::new(
                "The media encryption info are absent from the given encrypted attachment",
            ));
        };

        let encrypted_data: &[u8] = attachment.encrypted_data.as_slice();

        let mut cursor = Cursor::new(encrypted_data);
        let mut decryptor =
            matrix_sdk_crypto::AttachmentDecryptor::new(&mut cursor, media_encryption_info)?;

        let mut decrypted_data = Vec::new();
        decryptor.read_to_end(&mut decrypted_data)?;

        Ok(decrypted_data)
    }
}

/// An encrypted attachment, usually created from `Attachment.encrypt`.
#[wasm_bindgen]
#[derive(Debug)]
pub struct EncryptedAttachment {
    media_encryption_info: Option<matrix_sdk_crypto::MediaEncryptionInfo>,
    encrypted_data: Vec<u8>,
}

#[wasm_bindgen]
impl EncryptedAttachment {
    /// Create a new encrypted attachment manually.
    ///
    /// It needs encrypted data, stored in an `Uint8Array`, and a
    /// [media encryption
    /// information](https://docs.rs/matrix-sdk-crypto/latest/matrix_sdk_crypto/struct.MediaEncryptionInfo.html),
    /// as a JSON-encoded string.
    ///
    /// The media encryption information aren't stored as a string:
    /// they are parsed, validated and fully deserialized.
    ///
    /// See [the specification to learn
    /// more](https://spec.matrix.org/unstable/client-server-api/#extensions-to-mroommessage-msgtypes).
    #[wasm_bindgen(constructor)]
    pub fn new(
        encrypted_data: Vec<u8>,
        media_encryption_info: &str,
    ) -> Result<EncryptedAttachment, JsError> {
        Ok(Self {
            encrypted_data,
            media_encryption_info: Some(serde_json::from_str(media_encryption_info)?),
        })
    }

    /// The actual encrypted data.
    ///
    /// **Warning**: It returns a **copy** of the entire encrypted
    /// data; be nice with your memory.
    #[wasm_bindgen(getter, js_name = "encryptedData")]
    pub fn encrypted_data(&self) -> Vec<u8> {
        self.encrypted_data.clone()
    }

    /// Return the media encryption info as a JSON-encoded string. The
    /// structure is fully valid.
    ///
    /// If the media encryption info have been consumed already, it
    /// will return `null`.
    #[wasm_bindgen(getter, js_name = "mediaEncryptionInfo")]
    pub fn media_encryption_info(&self) -> Option<String> {
        serde_json::to_string(self.media_encryption_info.as_ref()?).ok()
    }

    /// Check whether the media encryption info has been consumed by
    /// `Attachment.decrypt` already.
    #[wasm_bindgen(getter, js_name = "hasMediaEncryptionInfoBeenConsumed")]
    pub fn has_media_encryption_info_been_consumed(&self) -> bool {
        self.media_encryption_info.is_none()
    }
}
