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
    /// It produces an `EncryptedAttachment`, which can be used to
    /// retrieve the media encryption information, or the encrypted
    /// data.
    #[napi]
    pub fn encrypt(array: Uint8Array) -> napi::Result<EncryptedAttachment> {
        let buffer: &[u8] = array.deref();

        let mut cursor = Cursor::new(buffer);
        let mut encryptor = matrix_sdk_crypto::AttachmentEncryptor::new(&mut cursor);

        let mut encrypted_data = Vec::new();
        encryptor.read_to_end(&mut encrypted_data).map_err(into_err)?;

        let media_encryption_info = Some(encryptor.finish());

        Ok(EncryptedAttachment {
            encrypted_data: Uint8Array::new(encrypted_data),
            media_encryption_info,
        })
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
    #[napi]
    pub fn decrypt(attachment: &mut EncryptedAttachment) -> napi::Result<Uint8Array> {
        let media_encryption_info = match attachment.media_encryption_info.take() {
            Some(media_encryption_info) => media_encryption_info,
            None => {
                return Err(napi::Error::from_reason(
                    "The media encryption info are absent from the given encrypted attachment"
                        .to_owned(),
                ))
            }
        };

        let encrypted_data: &[u8] = attachment.encrypted_data.deref();

        let mut cursor = Cursor::new(encrypted_data);
        let mut decryptor =
            matrix_sdk_crypto::AttachmentDecryptor::new(&mut cursor, media_encryption_info)
                .map_err(into_err)?;

        let mut decrypted_data = Vec::new();
        decryptor.read_to_end(&mut decrypted_data).map_err(into_err)?;

        Ok(Uint8Array::new(decrypted_data))
    }
}

/// An encrypted attachment, usually created from `Attachment.encrypt`.
#[napi]
pub struct EncryptedAttachment {
    media_encryption_info: Option<matrix_sdk_crypto::MediaEncryptionInfo>,

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
    ///
    /// See [the specification to learn
    /// more](https://spec.matrix.org/unstable/client-server-api/#extensions-to-mroommessage-msgtypes).
    #[napi(constructor)]
    pub fn new(encrypted_data: Uint8Array, media_encryption_info: String) -> napi::Result<Self> {
        Ok(Self {
            encrypted_data,
            media_encryption_info: Some(
                serde_json::from_str(media_encryption_info.as_str()).map_err(into_err)?,
            ),
        })
    }

    /// Return the media encryption info as a JSON-encoded string. The
    /// structure is fully valid.
    ///
    /// If the media encryption info have been consumed already, it
    /// will return `null`.
    #[napi(getter)]
    pub fn media_encryption_info(&self) -> Option<String> {
        serde_json::to_string(self.media_encryption_info.as_ref()?).ok()
    }

    /// Check whether the media encryption info has been consumed by
    /// `Attachment.decrypt` already.
    #[napi(getter)]
    pub fn has_media_encryption_info_been_consumed(&self) -> bool {
        self.media_encryption_info.is_none()
    }
}
