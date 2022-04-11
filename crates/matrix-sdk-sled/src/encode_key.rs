use matrix_sdk_common::ruma::{
    events::{secret::request::SecretName, GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType},
    receipt::ReceiptType,
    EventEncryptionAlgorithm,
    DeviceId, EventId, MxcUri, RoomId, TransactionId, UserId,
};
use matrix_sdk_store_encryption::StoreCipher;


pub const ENCODE_SEPARATOR: u8 = 0xff;

pub trait EncodeKey {
    fn encode(&self) -> Vec<u8>;
}

impl<T: EncodeKey + ?Sized> EncodeKey for &T {
    fn encode(&self) -> Vec<u8> {
        T::encode(self)
    }
}

impl<T: EncodeKey + ?Sized> EncodeKey for Box<T> {
    fn encode(&self) -> Vec<u8> {
        T::encode(self)
    }
}

impl EncodeKey for str {
    fn encode(&self) -> Vec<u8> {
        [self.as_bytes(), &[ENCODE_SEPARATOR]].concat()
    }
}

impl EncodeKey for String {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for DeviceId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for EventId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for RoomId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for TransactionId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for MxcUri {
    fn encode(&self) -> Vec<u8> {
        let s: &str = self.as_ref();
        s.encode()
    }
}

impl EncodeKey for SecretName {
    fn encode(&self) -> Vec<u8> {
        let s: &str = self.as_ref();
        s.encode()
    }
}

impl EncodeKey for ReceiptType {
    fn encode(&self) -> Vec<u8> {
        let s: &str = self.as_ref();
        s.encode()
    }
}

impl EncodeKey for EventEncryptionAlgorithm {
    fn encode(&self) -> Vec<u8> {
        let s: &str = self.as_ref();
        s.encode()
    }
}

impl EncodeKey for RoomAccountDataEventType {
    fn encode(&self) -> Vec<u8> {
        self.to_string().encode()
    }
}

impl EncodeKey for UserId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl<A, B> EncodeKey for (A, B)
where
    A: EncodeKey,
    B: EncodeKey,
{
    fn encode(&self) -> Vec<u8> {
        [
            self.0.encode(),
            self.1.encode(),
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
            self.0.encode(),
            self.1.encode(),
            self.2.encode(),
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
            self.0.encode(),
            self.1.encode(),
            self.2.encode(),
            self.3.encode(),
        ]
        .concat()
    }
}

impl EncodeKey for StateEventType {
    fn encode(&self) -> Vec<u8> {
        self.to_string().encode()
    }
}

impl EncodeKey for GlobalAccountDataEventType {
    fn encode(&self) -> Vec<u8> {
        self.to_string().encode()
    }
}


pub trait EncodeSecureKey {
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8>;
}


impl<T: EncodeSecureKey + ?Sized> EncodeSecureKey for &T {
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        T::encode_secure(self, table_name, store_cipher)
    }
}

impl<T: EncodeSecureKey + ?Sized> EncodeSecureKey for Box<T> {
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        T::encode_secure(self, table_name, store_cipher)
    }
}


impl EncodeSecureKey for UserId {
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        let user_id = store_cipher.hash_key(table_name, self.as_bytes());

        [user_id.as_slice(), &[ENCODE_SEPARATOR]].concat()
    }
}

impl EncodeSecureKey for str {
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        let key = store_cipher.hash_key(table_name, self.as_bytes());
        [key.as_slice(), &[ENCODE_SEPARATOR]].concat()
    }
}

impl EncodeSecureKey for String {
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        let key = store_cipher.hash_key(table_name, self.as_bytes());
        [key.as_slice(), &[ENCODE_SEPARATOR]].concat()
    }
}

impl EncodeSecureKey for RoomId {
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        let room_id = store_cipher.hash_key(table_name, self.as_bytes());

        [room_id.as_slice(), &[ENCODE_SEPARATOR]].concat()
    }
}

impl EncodeSecureKey for EventId {
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        let event_id = store_cipher.hash_key(table_name, self.as_bytes());

        [event_id.as_slice(), &[ENCODE_SEPARATOR]].concat()
    }
}

impl EncodeSecureKey for MxcUri {
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        let s: &str = self.as_ref();
        [
            store_cipher.hash_key(table_name, s.as_bytes()).as_slice(),
            &[ENCODE_SEPARATOR],
        ].concat()
    }
}

impl EncodeSecureKey for ReceiptType {
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        let event_id = store_cipher.hash_key(table_name, self.as_str().as_bytes());

        [event_id.as_slice(), &[ENCODE_SEPARATOR]].concat()
    }
}

impl EncodeSecureKey for GlobalAccountDataEventType {
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        [
            store_cipher.hash_key(table_name, self.to_string().as_bytes()).as_slice(),
            &[ENCODE_SEPARATOR],
        ].concat()
    }
}

impl EncodeSecureKey for StateEventType {
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        [
            store_cipher.hash_key(table_name, self.to_string().as_bytes()).as_slice(),
            &[ENCODE_SEPARATOR],
        ].concat()
    }
}

impl EncodeSecureKey for RoomAccountDataEventType {
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        [
            store_cipher.hash_key(table_name, self.to_string().as_bytes()).as_slice(),
            &[ENCODE_SEPARATOR],
        ].concat()
    }
}


impl<A, B> EncodeSecureKey for (A, B)
where
    A: EncodeSecureKey,
    B: EncodeSecureKey,
{
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        [
            self.0.encode_secure(table_name, store_cipher),
            self.1.encode_secure(table_name, store_cipher),
        ]
        .concat()
    }
}

impl<A, B, C> EncodeSecureKey for (A, B, C)
where
    A: EncodeSecureKey,
    B: EncodeSecureKey,
    C: EncodeSecureKey,
{
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        [
            self.0.encode_secure(table_name, store_cipher),
            self.1.encode_secure(table_name, store_cipher),
            self.2.encode_secure(table_name, store_cipher),
        ]
        .concat()
    }
}

impl<A, B, C, D> EncodeSecureKey for (A, B, C, D)
where
    A: EncodeSecureKey,
    B: EncodeSecureKey,
    C: EncodeSecureKey,
    D: EncodeSecureKey,
{
    fn encode_secure(&self, table_name: &str, store_cipher: &StoreCipher) -> Vec<u8> {
        [
            self.0.encode_secure(table_name, store_cipher),
            self.1.encode_secure(table_name, store_cipher),
            self.2.encode_secure(table_name, store_cipher),
            self.3.encode_secure(table_name, store_cipher),
        ]
        .concat()
    }
}