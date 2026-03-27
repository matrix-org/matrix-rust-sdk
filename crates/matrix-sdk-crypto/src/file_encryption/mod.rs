mod attachments;
mod key_export;
#[cfg(feature = "stream-attachment-encryption")]
mod stream_attachments;

pub use attachments::{
    AttachmentDecryptor, AttachmentEncryptor, DecryptorError, MediaEncryptionInfo,
};
pub use key_export::{KeyExportError, decrypt_room_key_export, encrypt_room_key_export};
#[cfg(feature = "stream-attachment-encryption")]
pub use stream_attachments::{
    StreamAttachmentDecryptor, StreamAttachmentEncryptor, StreamDecryptorError,
    StreamMediaEncryptionInfo,
};
