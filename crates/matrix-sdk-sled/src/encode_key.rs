use matrix_sdk_common::ruma::{
    events::{secret::request::SecretName, GlobalAccountDataEventType, StateEventType},
    DeviceId, EventId, MxcUri, RoomId, TransactionId, UserId,
};
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

impl EncodeKey for UserId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl<A, B> EncodeKey for (A, B)
where
    A: AsRef<str>,
    B: AsRef<str>,
{
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_ref().as_bytes(),
            &[ENCODE_SEPARATOR],
            self.1.as_ref().as_bytes(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }
}

impl<A, B, C> EncodeKey for (A, B, C)
where
    A: AsRef<str>,
    B: AsRef<str>,
    C: AsRef<str>,
{
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_ref().as_bytes(),
            &[ENCODE_SEPARATOR],
            self.1.as_ref().as_bytes(),
            &[ENCODE_SEPARATOR],
            self.2.as_ref().as_bytes(),
            &[ENCODE_SEPARATOR],
        ]
        .concat()
    }
}

impl<A, B, C, D> EncodeKey for (A, B, C, D)
where
    A: AsRef<str>,
    B: AsRef<str>,
    C: AsRef<str>,
    D: AsRef<str>,
{
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_ref().as_bytes(),
            &[ENCODE_SEPARATOR],
            self.1.as_ref().as_bytes(),
            &[ENCODE_SEPARATOR],
            self.2.as_ref().as_bytes(),
            &[ENCODE_SEPARATOR],
            self.3.as_ref().as_bytes(),
            &[ENCODE_SEPARATOR],
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

#[cfg(feature = "state-store")]
pub fn encode_key_with_usize<A: AsRef<str>>(s: A, i: usize) -> Vec<u8> {
    // FIXME: Not portable across architectures
    [s.as_ref().as_bytes(), &[ENCODE_SEPARATOR], i.to_be_bytes().as_ref(), &[ENCODE_SEPARATOR]]
        .concat()
}
