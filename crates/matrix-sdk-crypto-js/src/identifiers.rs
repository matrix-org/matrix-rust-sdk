//! Types for [Matrix](https://matrix.org/) identifiers for devices,
//! events, keys, rooms, servers, users and URIs.

use crate::prelude::*;

/// A Matrix [user ID].
///
/// [user ID]: https://spec.matrix.org/v1.2/appendices/#user-identifiers
#[cfg_attr(feature = "js", wasm_bindgen)]
#[cfg_attr(feature = "nodejs", napi)]
pub struct UserId {
    pub(crate) inner: ruma::OwnedUserId,
}

#[cfg_attr(feature = "js", wasm_bindgen)]
impl UserId {
    /// Parse/validate and create a new `UserId`.
    #[cfg_attr(feature = "js", wasm_bindgen(constructor))]
    pub fn new(id: &str) -> Result<UserId, Error> {
        Ok(Self { inner: ruma::UserId::parse(id)? })
    }

    /// Returns the user's localpart.
    pub fn localpart(&self) -> String {
        self.inner.localpart().to_owned()
    }

    /// Returns the server name of the user ID.
    #[cfg_attr(feature = "js", wasm_bindgen(js_name = "serverName"))]
    pub fn server_name(&self) -> ServerName {
        ServerName { inner: self.inner.server_name().to_owned() }
    }

    /// Whether this user ID is a historical one.
    ///
    /// A historical user ID is one that doesn't conform to the latest
    /// specification of the user ID grammar but is still accepted
    /// because it was previously allowed.
    #[cfg_attr(feature = "js", wasm_bindgen(getter, js_name = "isHistorical"))]
    pub fn is_historical(&self) -> bool {
        self.inner.is_historical()
    }

    /// Return the user ID as a string.
    #[cfg_attr(feature = "js", wasm_bindgen(js_name = "toString"))]
    pub fn to_string(&self) -> String {
        self.inner.as_str().to_owned()
    }
}

#[cfg(feature = "nodejs")]
#[napi]
impl UserId {
    /// Parse/validate and create a new `UserId`.
    #[napi(constructor)]
    pub fn new_(id: String) -> Result<UserId, napi::Error> {
        Self::new(id.as_ref()).map_err(Into::<napi::Error>::into)
    }

    /// Returns the user's localpart.
    #[napi(js_name = "localpart")]
    pub fn localpart_(&self) -> String {
        self.localpart()
    }

    /// Returns the server name of the user ID.
    #[napi(js_name = "serverName")]
    pub fn server_name_(&self) -> ServerName {
        self.server_name()
    }

    /// Whether this user ID is a historical one.
    ///
    /// A historical user ID is one that doesn't conform to the latest
    /// specification of the user ID grammar but is still accepted
    /// because it was previously allowed.
    #[napi(getter, js_name = "isHistorical")]
    pub fn is_historical_(&self) -> bool {
        self.is_historical()
    }

    /// Return the user ID as a string.
    #[napi(js_name = "toString")]
    pub fn to_string_(&self) -> String {
        self.to_string()
    }
}

/// A Matrix key ID.
///
/// Device identifiers in Matrix are completely opaque character
/// sequences. This type is provided simply for its semantic value.
#[cfg_attr(feature = "js", wasm_bindgen)]
#[cfg_attr(feature = "nodejs", napi)]
#[derive(Debug, Clone)]
pub struct DeviceId {
    pub(crate) inner: ruma::OwnedDeviceId,
}

#[cfg_attr(feature = "js", wasm_bindgen)]
impl DeviceId {
    /// Create a new `DeviceId`.
    #[cfg_attr(feature = "js", wasm_bindgen(constructor))]
    pub fn new(id: &str) -> DeviceId {
        Self { inner: id.into() }
    }

    /// Return the device ID as a string.
    #[cfg_attr(feature = "js", wasm_bindgen(js_name = "toString"))]
    pub fn to_string(&self) -> String {
        self.inner.as_str().to_owned()
    }
}

#[cfg(feature = "nodejs")]
#[napi]
impl DeviceId {
    /// Create a new `DeviceId`.
    #[napi(constructor)]
    pub fn new_(id: String) -> DeviceId {
        Self::new(id.as_ref())
    }

    /// Return the device ID as a string.
    #[napi(js_name = "toString")]
    pub fn to_string_(&self) -> String {
        self.to_string()
    }
}

/// A Matrix [room ID].
///
/// [room ID]: https://spec.matrix.org/v1.2/appendices/#room-ids-and-event-ids
#[cfg_attr(feature = "js", wasm_bindgen)]
#[cfg_attr(feature = "nodejs", napi)]
#[derive(Debug, Clone)]
pub struct RoomId {
    pub(crate) inner: ruma::OwnedRoomId,
}

#[cfg_attr(feature = "js", wasm_bindgen)]
impl RoomId {
    /// Parse/validate and create a new `RoomId`.
    #[cfg_attr(feature = "js", wasm_bindgen(constructor))]
    pub fn new(id: &str) -> Result<RoomId, Error> {
        Ok(Self { inner: ruma::RoomId::parse(id)? })
    }

    /// Returns the user's localpart.
    pub fn localpart(&self) -> String {
        self.inner.localpart().to_owned()
    }

    /// Returns the server name of the room ID.
    #[cfg_attr(feature = "js", wasm_bindgen(js_name = "serverName"))]
    pub fn server_name(&self) -> ServerName {
        ServerName { inner: self.inner.server_name().to_owned() }
    }

    /// Return the room ID as a string.
    #[cfg_attr(feature = "js", wasm_bindgen(js_name = "toString"))]
    pub fn to_string(&self) -> String {
        self.inner.as_str().to_owned()
    }
}

#[cfg(feature = "nodejs")]
#[napi]
impl RoomId {
    /// Parse/validate and create a new `RoomId`.
    #[napi(constructor)]
    pub fn new_(id: String) -> Result<RoomId, napi::Error> {
        Self::new(id.as_ref()).map_err(Into::<napi::Error>::into)
    }

    /// Returns the user's localpart.
    #[napi(js_name = "localpart")]
    pub fn localpart_(&self) -> String {
        self.localpart()
    }

    /// Returns the server name of the room ID.
    #[napi(js_name = "serverName")]
    pub fn server_name_(&self) -> ServerName {
        self.server_name()
    }

    /// Return the room ID as a string.
    #[napi(js_name = "toString")]
    pub fn to_string_(&self) -> String {
        self.to_string()
    }
}

/// A Matrix-spec compliant [server name].
///
/// It consists of a host and an optional port (separated by a colon if
/// present).
///
/// [server name]: https://spec.matrix.org/v1.2/appendices/#server-name
#[cfg_attr(feature = "js", wasm_bindgen)]
#[cfg_attr(feature = "nodejs", napi)]
#[derive(Debug)]
pub struct ServerName {
    inner: ruma::OwnedServerName,
}

#[cfg_attr(feature = "js", wasm_bindgen)]
impl ServerName {
    /// Parse/validate and create a new `ServerName`.
    #[cfg_attr(feature = "js", wasm_bindgen(constructor))]
    pub fn new(name: &str) -> Result<ServerName, Error> {
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
    #[cfg_attr(feature = "js", wasm_bindgen(js_name = "isIpLiteral"))]
    pub fn is_ip_literal(&self) -> bool {
        self.inner.is_ip_literal()
    }
}

#[cfg(feature = "nodejs")]
#[napi]
impl ServerName {
    /// Parse/validate and create a new `ServerName`.
    #[napi(constructor)]
    pub fn new_(name: String) -> Result<ServerName, napi::Error> {
        Self::new(name.as_ref()).map_err(Into::<napi::Error>::into)
    }

    /// Returns the host of the server name.
    ///
    /// That is: Return the part of the server before `:<port>` or the
    /// full server name if there is no port.
    #[napi(js_name = "host")]
    pub fn host_(&self) -> String {
        self.host()
    }

    /// Returns the port of the server name if any.
    #[napi(js_name = "port")]
    pub fn port_(&self) -> Option<u16> {
        self.port()
    }

    /// Returns true if and only if the server name is an IPv4 or IPv6
    /// address.
    #[napi(js_name = "isIpLiteral")]
    pub fn is_ip_literal_(&self) -> bool {
        self.is_ip_literal()
    }
}
