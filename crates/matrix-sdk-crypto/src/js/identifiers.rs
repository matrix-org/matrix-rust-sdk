//! Types for [Matrix](https://matrix.org/) identifiers for devices,
//! events, keys, rooms, servers, users and URIs.

use wasm_bindgen::prelude::*;

/// A Matrix [user ID].
///
/// [user ID]: https://spec.matrix.org/v1.2/appendices/#user-identifiers
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct UserId {
    pub(crate) inner: ruma::OwnedUserId,
}

#[wasm_bindgen]
impl UserId {
    /// Parse/validate and create a new `UserId`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: &str) -> Result<UserId, JsError> {
        Ok(Self { inner: ruma::UserId::parse(id)? })
    }

    /// Returns the user's localpart.
    pub fn localpart(&self) -> String {
        self.inner.localpart().to_owned()
    }

    /// Returns the server name of the user ID.
    #[wasm_bindgen(js_name = "serverName")]
    pub fn server_name(&self) -> ServerName {
        ServerName { inner: self.inner.server_name().to_owned() }
    }

    /// Whether this user ID is a historical one.
    ///
    /// A historical user ID is one that doesn't conform to the latest
    /// specification of the user ID grammar but is still accepted
    /// because it was previously allowed.
    #[wasm_bindgen(getter, js_name = "isHistorical")]
    pub fn is_historical(&self) -> bool {
        self.inner.is_historical()
    }

    /// Return the user ID as a string.
    #[wasm_bindgen(js_name = "toString")]
    pub fn to_string(&self) -> String {
        self.inner.as_str().to_owned()
    }
}

/// A Matrix key ID.
///
/// Device identifiers in Matrix are completely opaque character
/// sequences. This type is provided simply for its semantic value.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct DeviceId {
    pub(crate) inner: ruma::OwnedDeviceId,
}

#[wasm_bindgen]
impl DeviceId {
    /// Create a new `DeviceId`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: &str) -> DeviceId {
        Self { inner: id.into() }
    }

    /// Return the device ID as a string.
    #[wasm_bindgen(js_name = "toString")]
    pub fn to_string(&self) -> String {
        self.inner.as_str().to_owned()
    }
}

/// A Matrix [room ID].
///
/// [room ID]: https://spec.matrix.org/v1.2/appendices/#room-ids-and-event-ids
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct RoomId {
    pub(crate) inner: ruma::OwnedRoomId,
}

#[wasm_bindgen]
impl RoomId {
    /// Parse/validate and create a new `UserId`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: &str) -> Result<RoomId, JsError> {
        Ok(Self { inner: ruma::RoomId::parse(id)? })
    }

    /// Returns the user's localpart.
    pub fn localpart(&self) -> String {
        self.inner.localpart().to_owned()
    }

    /// Returns the server name of the user ID.
    #[wasm_bindgen(js_name = "serverName")]
    pub fn server_name(&self) -> ServerName {
        ServerName { inner: self.inner.server_name().to_owned() }
    }

    /// Return the device ID as a string.
    #[wasm_bindgen(js_name = "toString")]
    pub fn to_string(&self) -> String {
        self.inner.as_str().to_owned()
    }
}

/// A Matrix-spec compliant [server name].
///
/// It consists of a host and an optional port (separated by a colon if
/// present).
///
/// [server name]: https://spec.matrix.org/v1.2/appendices/#server-name
#[wasm_bindgen]
#[derive(Debug)]
pub struct ServerName {
    inner: ruma::OwnedServerName,
}

#[wasm_bindgen]
impl ServerName {
    /// Parse/validate and create a new `ServerName`.
    #[wasm_bindgen(constructor)]
    pub fn new(name: &str) -> Result<ServerName, JsError> {
        Ok(Self { inner: ruma::ServerName::parse(name)? })
    }

    /// Returns the host of the server name.
    ///
    /// That is: Return the part of the server before `:<port>` or the
    /// full server name if there is no port.
    pub fn host(&self) -> String {
        self.inner.host().to_owned()
    }

    /// Returns the port of the server name if any.
    pub fn port(&self) -> Option<u16> {
        self.inner.port()
    }

    /// Returns true if and only if the server name is an IPv4 or IPv6
    /// address.
    #[wasm_bindgen(js_name = "isIpLiteral")]
    pub fn is_ip_literal(&self) -> bool {
        self.inner.is_ip_literal()
    }
}
