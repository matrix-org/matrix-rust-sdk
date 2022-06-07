//! Types for [Matrix](https://matrix.org/) identifiers for devices,
//! events, keys, rooms, servers, users and URIs.

use napi::Result;
use napi_derive::*;

use crate::into_err;

/// A Matrix [user ID].
///
/// [user ID]: https://spec.matrix.org/v1.2/appendices/#user-identifiers
#[napi]
#[derive(Debug, Clone)]
pub struct UserId {
    pub(crate) inner: ruma::OwnedUserId,
}

impl UserId {
    pub(crate) fn new_with(inner: ruma::OwnedUserId) -> Self {
        Self { inner }
    }
}

#[napi]
impl UserId {
    /// Parse/validate and create a new `UserId`.
    #[napi(constructor)]
    pub fn new(id: String) -> Result<UserId> {
        Ok(Self::new_with(ruma::UserId::parse(id.as_str()).map_err(into_err)?))
    }

    /// Returns the user's localpart.
    #[napi(getter)]
    pub fn localpart(&self) -> String {
        self.inner.localpart().to_owned()
    }

    /// Returns the server name of the user ID.
    #[napi(getter)]
    pub fn server_name(&self) -> ServerName {
        ServerName { inner: self.inner.server_name().to_owned() }
    }

    /// Whether this user ID is a historical one.
    ///
    /// A historical user ID is one that doesn't conform to the latest
    /// specification of the user ID grammar but is still accepted
    /// because it was previously allowed.
    #[napi]
    pub fn is_historical(&self) -> bool {
        self.inner.is_historical()
    }

    /// Return the user ID as a string.
    #[napi]
    #[allow(clippy::inherent_to_string)]
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

impl DeviceId {
    pub(crate) fn new_with(inner: ruma::OwnedDeviceId) -> Self {
        Self { inner }
    }
}

#[napi]
impl DeviceId {
    /// Create a new `DeviceId`.
    #[napi(constructor)]
    pub fn new(id: String) -> DeviceId {
        Self::new_with(id.into())
    }

    /// Return the device ID as a string.
    #[napi]
    #[allow(clippy::inherent_to_string)]
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

impl RoomId {
    pub(crate) fn new_with(inner: ruma::OwnedRoomId) -> Self {
        Self { inner }
    }
}

#[napi]
impl RoomId {
    /// Parse/validate and create a new `RoomId`.
    #[napi(constructor)]
    pub fn new(id: String) -> Result<RoomId> {
        Ok(Self::new_with(ruma::RoomId::parse(id).map_err(into_err)?))
    }

    /// Returns the user's localpart.
    #[napi(getter)]
    pub fn localpart(&self) -> String {
        self.inner.localpart().to_owned()
    }

    /// Returns the server name of the room ID.
    #[napi(getter)]
    pub fn server_name(&self) -> ServerName {
        ServerName { inner: self.inner.server_name().to_owned() }
    }

    /// Return the room ID as a string.
    #[napi]
    #[allow(clippy::inherent_to_string)]
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
    pub fn new(name: String) -> Result<ServerName> {
        Ok(Self { inner: ruma::ServerName::parse(name).map_err(into_err)? })
    }

    /// Returns the host of the server name.
    ///
    /// That is: Return the part of the server before `:<port>` or the
    /// full server name if there is no port.
    #[napi(getter)]
    pub fn host(&self) -> String {
        self.inner.host().to_owned()
    }

    /// Returns the port of the server name if any.
    #[napi(getter)]
    pub fn port(&self) -> Option<u16> {
        self.inner.port()
    }

    /// Returns true if and only if the server name is an IPv4 or IPv6
    /// address.
    #[napi]
    pub fn is_ip_literal(&self) -> bool {
        self.inner.is_ip_literal()
    }
}
