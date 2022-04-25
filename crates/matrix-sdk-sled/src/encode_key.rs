use std::{borrow::Cow, ops::Deref};

use matrix_sdk_common::ruma::{
    events::{
        secret::request::SecretName, GlobalAccountDataEventType, RoomAccountDataEventType,
        StateEventType,
    },
    receipt::ReceiptType,
    DeviceId, EventEncryptionAlgorithm, EventId, MxcUri, RoomId, TransactionId, UserId,
};
use matrix_sdk_store_encryption::StoreCipher;

pub const ENCODE_SEPARATOR: u8 = 0xff;

pub trait EncodeKey {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        unimplemented!()
    }

    fn encode(&self) -> Vec<u8> {
        [self.encode_as_bytes().deref(), &[ENCODE_SEPARATOR]].concat()
    }
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        let key = store_cipher.hash_key(table_name, &self.encode_as_bytes());
        [key.as_slice(), &[ENCODE_SEPARATOR]].concat()
    }
}

impl<T: EncodeKey + ?Sized> EncodeKey for &T {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        T::encode_as_bytes(self)
    }
    fn encode(&self) -> Vec<u8> {
        T::encode(self)
    }
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        T::encode_secure(self, table_name, store_cipher)
    }
}

impl<T: EncodeKey + ?Sized> EncodeKey for Box<T> {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        T::encode_as_bytes(self)
    }
    fn encode(&self) -> Vec<u8> {
        T::encode(self)
    }
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        T::encode_secure(self, table_name, store_cipher)
    }
}

impl EncodeKey for str {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        self.as_bytes().into()
    }
}

impl EncodeKey for String {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        self.as_str().as_bytes().into()
    }
}

impl EncodeKey for DeviceId {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        self.as_str().as_bytes().into()
    }
}

impl EncodeKey for EventId {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        self.as_str().as_bytes().into()
    }
}

impl EncodeKey for RoomId {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        self.as_str().as_bytes().into()
    }
}

impl EncodeKey for TransactionId {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        self.as_str().as_bytes().into()
    }
}

impl EncodeKey for MxcUri {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        let s: &str = self.as_ref();
        s.as_bytes().into()
    }
}

impl EncodeKey for SecretName {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        let s: &str = self.as_ref();
        s.as_bytes().into()
    }
}

impl EncodeKey for ReceiptType {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        let s: &str = self.as_ref();
        s.as_bytes().into()
    }
}

impl EncodeKey for EventEncryptionAlgorithm {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        let s: &str = self.as_ref();
        s.as_bytes().into()
    }
}

impl EncodeKey for RoomAccountDataEventType {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        self.to_string().as_bytes().to_vec().into()
    }
}

impl EncodeKey for UserId {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        self.as_str().as_bytes().into()
    }
}

impl EncodeKey for StateEventType {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        self.to_string().as_bytes().to_vec().into()
    }
}

impl EncodeKey for GlobalAccountDataEventType {
    fn encode_as_bytes(&self) -> Cow<'_, [u8]> {
        self.to_string().as_bytes().to_vec().into()
    }
}

impl<A, B> EncodeKey for (A, B)
where
    A: EncodeKey,
    B: EncodeKey,
{
    fn encode(&self) -> Vec<u8> {
        [
            self.0.encode_as_bytes().deref(),
            &[ENCODE_SEPARATOR],
            self.1.encode_as_bytes().deref(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }

    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        [
            store_cipher.hash_key(table_name, &self.0.encode_as_bytes()).as_slice(),
            &[ENCODE_SEPARATOR],
            store_cipher.hash_key(table_name, &self.1.encode_as_bytes()).as_slice(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }
}

impl<A, B, C> EncodeKey for (A, B, C)
where
    A: EncodeKey,
    B: EncodeKey,
    C: EncodeKey,
{
    fn encode(&self) -> Vec<u8> {
        [
            self.0.encode_as_bytes().deref(),
            &[ENCODE_SEPARATOR],
            self.1.encode_as_bytes().deref(),
            &[ENCODE_SEPARATOR],
            self.2.encode_as_bytes().deref(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }

    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        [
            store_cipher.hash_key(table_name, &self.0.encode_as_bytes()).as_slice(),
            &[ENCODE_SEPARATOR],
            store_cipher.hash_key(table_name, &self.1.encode_as_bytes()).as_slice(),
            &[ENCODE_SEPARATOR],
            store_cipher.hash_key(table_name, &self.2.encode_as_bytes()).as_slice(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }
}

impl<A, B, C, D> EncodeKey for (A, B, C, D)
where
    A: EncodeKey,
    B: EncodeKey,
    C: EncodeKey,
    D: EncodeKey,
{
    fn encode(&self) -> Vec<u8> {
        [
            self.0.encode_as_bytes().deref(),
            &[ENCODE_SEPARATOR],
            self.1.encode_as_bytes().deref(),
            &[ENCODE_SEPARATOR],
            self.2.encode_as_bytes().deref(),
            &[ENCODE_SEPARATOR],
            self.3.encode_as_bytes().deref(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }

    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        [
            store_cipher.hash_key(table_name, &self.0.encode_as_bytes()).as_slice(),
            &[ENCODE_SEPARATOR],
            store_cipher.hash_key(table_name, &self.1.encode_as_bytes()).as_slice(),
            &[ENCODE_SEPARATOR],
            store_cipher.hash_key(table_name, &self.2.encode_as_bytes()).as_slice(),
            &[ENCODE_SEPARATOR],
            store_cipher.hash_key(table_name, &self.3.encode_as_bytes()).as_slice(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }
}
