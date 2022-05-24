//! Types for [Matrix](https://matrix.org/) identifiers for devices,
//! events, keys, rooms, servers, users and URIs.

use napi_derive::*;

use crate::errors::*;

/// A Matrix [user ID].
///
/// [user ID]: https://spec.matrix.org/v1.2/appendices/#user-identifiers
#[napi]
pub struct UserId {
    pub(crate) inner: ruma::OwnedUserId,
}

#[napi]
impl UserId {
    /// Parse/validate and create a new `UserId`.
    #[napi(constructor)]
    pub fn new(id: String) -> Result<UserId, napi::Error> {
        Ok(Self {
            inner: ruma::UserId::parse(id.as_str())
                .map_err(Error::from)
                .map_err(Into::<napi::Error>::into)?,
        })
    }

    /// Returns the user's localpart.
    #[napi]
    pub fn localpart(&self) -> String {
        self.inner.localpart().to_owned()
    }

    /// Returns the server name of the user ID.
    #[napi(js_name = "serverName")]
    pub fn server_name(&self) -> ServerName {
        ServerName { inner: self.inner.server_name().to_owned() }
    }

    /// Whether this user ID is a historical one.
    ///
    /// A historical user ID is one that doesn't conform to the latest
    /// specification of the user ID grammar but is still accepted
    /// because it was previously allowed.
    #[napi(getter, js_name = "isHistorical")]
    pub fn is_historical(&self) -> bool {
        self.inner.is_historical()
    }

    /// Return the user ID as a string.
    #[napi(js_name = "toString")]
    pub fn to_string(&self) -> String {
        self.inner.as_str().to_owned()
    }
}

/// A Matrix key ID.
///
/// Device identifiers in Matrix are completely opaque character
/// sequences. This type is provided simply for its semantic value.
#[napi]
#[derive(Debug, Clone)]
pub struct DeviceId {
    pub(crate) inner: ruma::OwnedDeviceId,
}

#[napi]
impl DeviceId {
    /// Create a new `DeviceId`.
    #[napi(constructor)]
    pub fn new(id: String) -> DeviceId {
        Self { inner: id.into() }
    }

    /// Return the device ID as a string.
    #[napi(js_name = "toString")]
    pub fn to_string(&self) -> String {
        self.inner.as_str().to_owned()
    }
}

/// A Matrix [room ID].
///
/// [room ID]: https://spec.matrix.org/v1.2/appendices/#room-ids-and-event-ids
#[napi]
#[derive(Debug, Clone)]
pub struct RoomId {
    pub(crate) inner: ruma::OwnedRoomId,
}

#[napi]
impl RoomId {
    /// Parse/validate and create a new `RoomId`.
    #[napi(constructor)]
    pub fn new(id: String) -> Result<RoomId, napi::Error> {
        Ok(Self {
            inner: ruma::RoomId::parse(id)
                .map_err(Error::from)
                .map_err(Into::<napi::Error>::into)?,
        })
    }

    /// Returns the user's localpart.
    #[napi]
    pub fn localpart(&self) -> String {
        self.inner.localpart().to_owned()
    }

    /// Returns the server name of the room ID.
    #[napi(js_name = "serverName")]
    pub fn server_name(&self) -> ServerName {
        ServerName { inner: self.inner.server_name().to_owned() }
    }

    /// Return the room ID as a string.
    #[napi(js_name = "toString")]
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
#[napi]
#[derive(Debug)]
pub struct ServerName {
    inner: ruma::OwnedServerName,
}

#[napi]
impl ServerName {
    /// Parse/validate and create a new `ServerName`.
    #[napi(constructor)]
    pub fn new(name: String) -> Result<ServerName, napi::Error> {
        Ok(Self {
            inner: ruma::ServerName::parse(name)
                .map_err(Error::from)
                .map_err(Into::<napi::Error>::into)?,
        })
    }

    /// Returns the host of the server name.
    ///
    /// That is: Return the part of the server before `:<port>` or the
    /// full server name if there is no port.
    #[napi]
    pub fn host(&self) -> String {
        self.inner.host().to_owned()
    }

    /// Returns the port of the server name if any.
    #[napi]
    pub fn port(&self) -> Option<u16> {
        self.inner.port()
    }

    /// Returns true if and only if the server name is an IPv4 or IPv6
    /// address.
    #[napi(js_name = "isIpLiteral")]
    pub fn is_ip_literal(&self) -> bool {
        self.inner.is_ip_literal()
    }
}
