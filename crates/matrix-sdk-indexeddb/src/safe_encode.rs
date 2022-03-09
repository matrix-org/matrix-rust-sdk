#![allow(dead_code)]
use matrix_sdk_common::ruma::{
    events::EventType, receipt::ReceiptType, DeviceId, EventId, MxcUri, RoomId, TransactionId,
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

    /// encode self into a JsValue, internally using `as_encoded_string`
    /// to escape the value of self.
    fn encode(&self) -> JsValue {
        JsValue::from(self.as_encoded_string())
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
}

/// Implement SafeEncode for tuple of two references, separating the escaped
/// values with with `KEY_SEPARATOR`.
impl<A, B> SafeEncode for (&A, &B)
where
    A: SafeEncode + ?Sized,
    B: SafeEncode + ?Sized,
{
    fn as_encoded_string(&self) -> String {
        [&self.0.as_encoded_string(), KEY_SEPARATOR, &self.1.as_encoded_string()].concat()
    }
}

/// Implement SafeEncode for tuple of three references, separating the escaped
/// values with with `KEY_SEPARATOR`.
impl<A, B, C> SafeEncode for (&A, &B, &C)
where
    A: SafeEncode + ?Sized,
    B: SafeEncode + ?Sized,
    C: SafeEncode + ?Sized,
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
}

/// Implement SafeEncode for tuple of four references, separating the escaped
/// values with with `KEY_SEPARATOR`.
impl<A, B, C, D> SafeEncode for (&A, &B, &C, &D)
where
    A: SafeEncode + ?Sized,
    B: SafeEncode + ?Sized,
    C: SafeEncode + ?Sized,
    D: SafeEncode + ?Sized,
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

impl<T: SafeEncode + ?Sized> SafeEncode for Box<T> {
    fn as_encoded_string(&self) -> String {
        (&**self).as_encoded_string()
    }
}

impl SafeEncode for TransactionId {
    fn as_encoded_string(&self) -> String {
        self.to_string()
    }
}

impl SafeEncode for EventType {
    fn as_encoded_string(&self) -> String {
        self.as_str().as_encoded_string()
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

impl SafeEncode for UserId {
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

impl SafeEncode for MxcUri {
    fn as_encoded_string(&self) -> String {
        self.as_str().as_encoded_string()
    }
}

impl SafeEncode for usize {
    fn as_encoded_string(&self) -> String {
        self.to_string()
    }
}
