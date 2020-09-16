#[allow(dead_code)]
mod attachments;
mod key_export;

pub use attachments::{AttachmentDecryptor, AttachmentEncryptor, DecryptorError};
pub use key_export::{decrypt_key_export, encrypt_key_export};

use base64::{decode_config, encode_config, DecodeError, STANDARD_NO_PAD, URL_SAFE_NO_PAD};

fn decode(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    decode_config(input, STANDARD_NO_PAD)
}

fn decode_url_safe(input: impl AsRef<[u8]>) -> Result<Vec<u8>, DecodeError> {
    decode_config(input, URL_SAFE_NO_PAD)
}

fn encode(input: impl AsRef<[u8]>) -> String {
    encode_config(input, STANDARD_NO_PAD)
}

fn encode_url_safe(input: impl AsRef<[u8]>) -> String {
    encode_config(input, URL_SAFE_NO_PAD)
}
