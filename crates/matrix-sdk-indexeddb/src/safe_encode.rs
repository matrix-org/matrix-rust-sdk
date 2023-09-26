#![allow(dead_code)]
use base64::{
    alphabet,
    engine::{general_purpose, GeneralPurpose},
    Engine,
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{
    events::{
        receipt::ReceiptType, GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
    },
    DeviceId, EventId, MxcUri, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, TransactionId,
    UserId,
};
use wasm_bindgen::JsValue;
use web_sys::IdbKeyRange;

/// Helpers for wasm32/browser environments

/// ASCII Group Separator, for elements in the keys
pub const KEY_SEPARATOR: &str = "\u{001D}";
/// ASCII Record Separator is sure smaller than the Key Separator but smaller
/// than regular characters
pub const RANGE_END: &str = "\u{001E}";
/// Using the literal escape character to escape KEY_SEPARATOR in regular keys
/// (though super unlikely)
pub const ESCAPED: &str = "\u{001E}\u{001D}";

const STANDARD_NO_PAD: GeneralPurpose =
    GeneralPurpose::new(&alphabet::STANDARD, general_purpose::NO_PAD);

/// Encode value as String/JsValue/IdbKeyRange for the JS APIs in a
/// safe, escaped manner.
///
/// Primary use is as a helper to escape potentially harmful opaque strings
/// from UserId, RoomId, etc into keys that can be used (also for ranges)
/// with the IndexedDB.
pub trait SafeEncode {
    /// Encode into a safe, escaped String
    ///
    /// It's the implementors responsibility to provide an encoded, safe
    /// string where `KEY_SEPARATOR` is escaped with the `ESCAPED`.
    /// The result will not be escaped again.
    fn as_encoded_string(&self) -> String;

    /// encode self securely for the given tablename with the given
    /// `store_cipher` hash_key, returns the value as a base64 encoded
    /// string without any padding.
    fn as_secure_string(&self, table_name: &str, store_cipher: &StoreCipher) -> String {
        STANDARD_NO_PAD
            .encode(store_cipher.hash_key(table_name, self.as_encoded_string().as_bytes()))
    }

    /// encode self into a JsValue, internally using `as_encoded_string`
    /// to escape the value of self, and append the given counter
    fn encode_with_counter(&self, i: usize) -> JsValue {
        format!("{}{KEY_SEPARATOR}{i:016x}", self.as_encoded_string()).into()
    }

    /// encode self into a JsValue, internally using `as_secure_string`
    /// to escape the value of self, and append the given counter
    fn encode_with_counter_secure(
        &self,
        table_name: &str,
        store_cipher: &StoreCipher,
        i: usize,
    ) -> JsValue {
        format!("{}{KEY_SEPARATOR}{i:016x}", self.as_secure_string(table_name, store_cipher)).into()
    }

    /// Encode self into a IdbKeyRange for searching all keys that are
    /// prefixed with this key, followed by `KEY_SEPARATOR`. Internally
    /// uses `as_encoded_string` to ensure the given key is escaped properly.
    fn encode_to_range(&self) -> Result<IdbKeyRange, String> {
        let key = self.as_encoded_string();
        IdbKeyRange::bound(
            &JsValue::from([&key, KEY_SEPARATOR].concat()),
            &JsValue::from([&key, RANGE_END].concat()),
        )
        .map_err(|e| e.as_string().unwrap_or_else(|| "Creating key range failed".to_owned()))
    }

    fn encode_to_range_secure(
        &self,
        table_name: &str,
        store_cipher: &StoreCipher,
    ) -> Result<IdbKeyRange, String> {
        let key = self.as_secure_string(table_name, store_cipher);
        IdbKeyRange::bound(
            &JsValue::from([&key, KEY_SEPARATOR].concat()),
            &JsValue::from([&key, RANGE_END].concat()),
        )
        .map_err(|e| e.as_string().unwrap_or_else(|| "Creating key range failed".to_owned()))
    }
}

/// Implement SafeEncode for tuple of two elements, separating the escaped
/// values with with `KEY_SEPARATOR`.
impl<A, B> SafeEncode for (A, B)
where
    A: SafeEncode,
    B: SafeEncode,
{
    fn as_encoded_string(&self) -> String {
        [&self.0.as_encoded_string(), KEY_SEPARATOR, &self.1.as_encoded_string()].concat()
    }

    fn as_secure_string(&self, table_name: &str, store_cipher: &StoreCipher) -> String {
        [
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.0.as_encoded_string().as_bytes())),
            KEY_SEPARATOR,
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.1.as_encoded_string().as_bytes())),
        ]
        .concat()
    }
}

/// Implement SafeEncode for tuple of three elements, separating the escaped
/// values with with `KEY_SEPARATOR`.
impl<A, B, C> SafeEncode for (A, B, C)
where
    A: SafeEncode,
    B: SafeEncode,
    C: SafeEncode,
{
    fn as_encoded_string(&self) -> String {
        [
            &self.0.as_encoded_string(),
            KEY_SEPARATOR,
            &self.1.as_encoded_string(),
            KEY_SEPARATOR,
            &self.2.as_encoded_string(),
        ]
        .concat()
    }

    fn as_secure_string(&self, table_name: &str, store_cipher: &StoreCipher) -> String {
        [
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.0.as_encoded_string().as_bytes())),
            KEY_SEPARATOR,
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.1.as_encoded_string().as_bytes())),
            KEY_SEPARATOR,
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.2.as_encoded_string().as_bytes())),
        ]
        .concat()
    }
}

/// Implement SafeEncode for tuple of four elements, separating the escaped
/// values with with `KEY_SEPARATOR`.
impl<A, B, C, D> SafeEncode for (A, B, C, D)
where
    A: SafeEncode,
    B: SafeEncode,
    C: SafeEncode,
    D: SafeEncode,
{
    fn as_encoded_string(&self) -> String {
        [
            &self.0.as_encoded_string(),
            KEY_SEPARATOR,
            &self.1.as_encoded_string(),
            KEY_SEPARATOR,
            &self.2.as_encoded_string(),
            KEY_SEPARATOR,
            &self.3.as_encoded_string(),
        ]
        .concat()
    }

    fn as_secure_string(&self, table_name: &str, store_cipher: &StoreCipher) -> String {
        [
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.0.as_encoded_string().as_bytes())),
            KEY_SEPARATOR,
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.1.as_encoded_string().as_bytes())),
            KEY_SEPARATOR,
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.2.as_encoded_string().as_bytes())),
            KEY_SEPARATOR,
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.3.as_encoded_string().as_bytes())),
        ]
        .concat()
    }
}

/// Implement SafeEncode for tuple of five elements, separating the escaped
/// values with with `KEY_SEPARATOR`.
impl<A, B, C, D, E> SafeEncode for (A, B, C, D, E)
where
    A: SafeEncode,
    B: SafeEncode,
    C: SafeEncode,
    D: SafeEncode,
    E: SafeEncode,
{
    fn as_encoded_string(&self) -> String {
        [
            &self.0.as_encoded_string(),
            KEY_SEPARATOR,
            &self.1.as_encoded_string(),
            KEY_SEPARATOR,
            &self.2.as_encoded_string(),
            KEY_SEPARATOR,
            &self.3.as_encoded_string(),
            KEY_SEPARATOR,
            &self.4.as_encoded_string(),
        ]
        .concat()
    }

    fn as_secure_string(&self, table_name: &str, store_cipher: &StoreCipher) -> String {
        [
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.0.as_encoded_string().as_bytes())),
            KEY_SEPARATOR,
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.1.as_encoded_string().as_bytes())),
            KEY_SEPARATOR,
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.2.as_encoded_string().as_bytes())),
            KEY_SEPARATOR,
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.3.as_encoded_string().as_bytes())),
            KEY_SEPARATOR,
            &STANDARD_NO_PAD
                .encode(store_cipher.hash_key(table_name, self.4.as_encoded_string().as_bytes())),
        ]
        .concat()
    }
}

impl SafeEncode for String {
    fn as_encoded_string(&self) -> String {
        self.replace(KEY_SEPARATOR, ESCAPED)
    }
}

impl SafeEncode for str {
    fn as_encoded_string(&self) -> String {
        self.replace(KEY_SEPARATOR, ESCAPED)
    }
}

impl<T: SafeEncode + ?Sized> SafeEncode for &T {
    fn as_encoded_string(&self) -> String {
        (*self).as_encoded_string()
    }
}

impl SafeEncode for TransactionId {
    fn as_encoded_string(&self) -> String {
        self.to_string().as_encoded_string()
    }
}

impl SafeEncode for GlobalAccountDataEventType {
    fn as_encoded_string(&self) -> String {
        self.to_string().as_encoded_string()
    }
}

impl SafeEncode for RoomAccountDataEventType {
    fn as_encoded_string(&self) -> String {
        self.to_string().as_encoded_string()
    }
}

impl SafeEncode for StateEventType {
    fn as_encoded_string(&self) -> String {
        self.to_string().as_encoded_string()
    }
}

impl SafeEncode for ReceiptType {
    fn as_encoded_string(&self) -> String {
        self.as_str().as_encoded_string()
    }
}

impl SafeEncode for RoomId {
    fn as_encoded_string(&self) -> String {
        self.as_str().as_encoded_string()
    }
}

impl SafeEncode for OwnedRoomId {
    fn as_encoded_string(&self) -> String {
        self.as_str().as_encoded_string()
    }
}

impl SafeEncode for UserId {
    fn as_encoded_string(&self) -> String {
        self.as_str().as_encoded_string()
    }
}

impl SafeEncode for OwnedUserId {
    fn as_encoded_string(&self) -> String {
        self.as_str().as_encoded_string()
    }
}

impl SafeEncode for DeviceId {
    fn as_encoded_string(&self) -> String {
        self.as_str().as_encoded_string()
    }
}

impl SafeEncode for EventId {
    fn as_encoded_string(&self) -> String {
        self.as_str().as_encoded_string()
    }
}

impl SafeEncode for OwnedEventId {
    fn as_encoded_string(&self) -> String {
        self.as_str().as_encoded_string()
    }
}

impl SafeEncode for MxcUri {
    fn as_encoded_string(&self) -> String {
        self.as_str().as_encoded_string()
    }
}

impl SafeEncode for usize {
    fn as_encoded_string(&self) -> String {
        self.to_string()
    }

    fn as_secure_string(&self, _table_name: &str, _store_cipher: &StoreCipher) -> String {
        self.to_string()
    }
}
