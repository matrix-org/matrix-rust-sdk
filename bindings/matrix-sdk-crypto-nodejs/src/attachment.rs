use std::{
    io::{Cursor, Read},
    ops::Deref,
};

use napi::bindgen_prelude::Uint8Array;
use napi_derive::*;

use crate::into_err;

/// A type to encrypt and to decrypt anything that can fit in an
/// `Uint8Array`, usually big buffer.
#[napi]
pub struct Attachment;

#[napi]
impl Attachment {
    /// Encrypt the content of the `Uint8Array`.
    ///
    /// It produces an `EncryptedAttachment`, we can be used to
    /// retrieve the media encryption information, or the encrypted
    /// data.
    #[napi]
    pub fn encrypt(array: Uint8Array) -> napi::Result<EncryptedAttachment> {
        let buffer: &[u8] = array.deref();

        let mut cursor = Cursor::new(buffer);
        let mut encryptor = matrix_sdk_crypto::AttachmentEncryptor::new(&mut cursor);

        let mut encrypted_data = Vec::new();
        encryptor.read_to_end(&mut encrypted_data).map_err(into_err)?;

        let media_encryption_info = encryptor.finish();

        Ok(EncryptedAttachment {
            encrypted_data: Uint8Array::new(encrypted_data),
            media_encryption_info,
        })
    }

    /// Decrypt an `EncryptedAttachment`.
    ///
    /// The encrypted attachment can be created manually, or from the
    /// `encrypt` method.
    #[napi]
    pub fn decrypt(attachment: &EncryptedAttachment) -> napi::Result<Uint8Array> {
        let encrypted_data: &[u8] = attachment.encrypted_data.deref();

        let mut cursor = Cursor::new(encrypted_data);
        let mut decryptor = matrix_sdk_crypto::AttachmentDecryptor::new(
            &mut cursor,
            attachment.media_encryption_info.clone(),
        )
        .map_err(into_err)?;

        let mut decrypted_data = Vec::new();
        decryptor.read_to_end(&mut decrypted_data).map_err(into_err)?;

        Ok(Uint8Array::new(decrypted_data))
    }
}

/// An encrypted attachment, usually created from `Attachment.encrypt`.
#[napi]
pub struct EncryptedAttachment {
    media_encryption_info: matrix_sdk_crypto::MediaEncryptionInfo,

    /// The actual encrypted data.
    pub encrypted_data: Uint8Array,
}

#[napi]
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
    #[napi(constructor)]
    pub fn new(encrypted_data: Uint8Array, media_encryption_info: String) -> napi::Result<Self> {
        Ok(Self {
            encrypted_data,
            media_encryption_info: serde_json::from_str(media_encryption_info.as_str())
                .map_err(into_err)?,
        })
    }

    /// Return the media encryption info as a JSON-encoded string. The
    /// structure is fully valid.
    #[napi(getter)]
    pub fn media_encryption_info(&self) -> String {
        serde_json::to_string(&self.media_encryption_info).unwrap()
    }
}
