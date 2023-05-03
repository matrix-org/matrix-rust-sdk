//! Types for [Matrix](https://matrix.org/) identifiers for devices,
//! events, keys, rooms, servers, users and URIs.

use napi::bindgen_prelude::{FromNapiValue, ToNapiValue};
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

impl From<ruma::OwnedUserId> for UserId {
    fn from(inner: ruma::OwnedUserId) -> Self {
        Self { inner }
    }
}

#[napi]
impl UserId {
    /// Parse/validate and create a new `UserId`.
    #[napi(constructor, strict)]
    pub fn new(id: String) -> napi::Result<Self> {
        Ok(Self::from(ruma::UserId::parse(id.as_str()).map_err(into_err)?))
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

/// A Matrix device ID.
///
/// Device identifiers in Matrix are completely opaque character
/// sequences. This type is provided simply for its semantic value.
#[napi]
#[derive(Debug, Clone)]
pub struct DeviceId {
    pub(crate) inner: ruma::OwnedDeviceId,
}

impl From<ruma::OwnedDeviceId> for DeviceId {
    fn from(inner: ruma::OwnedDeviceId) -> Self {
        Self { inner }
    }
}

#[napi]
impl DeviceId {
    /// Create a new `DeviceId`.
    #[napi(constructor, strict)]
    pub fn new(id: String) -> Self {
        Self::from(Into::<ruma::OwnedDeviceId>::into(id))
    }

    /// Return the device ID as a string.
    #[napi]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        self.inner.as_str().to_owned()
    }
}

/// A Matrix device key ID.
///
/// A key algorithm and a device ID, combined with a ‘:’.
#[napi]
#[derive(Debug, Clone)]
pub struct DeviceKeyId {
    pub(crate) inner: ruma::OwnedDeviceKeyId,
}

impl From<ruma::OwnedDeviceKeyId> for DeviceKeyId {
    fn from(inner: ruma::OwnedDeviceKeyId) -> Self {
        Self { inner }
    }
}

#[napi]
impl DeviceKeyId {
    /// Parse/validate and create a new `DeviceKeyId`.
    #[napi(constructor, strict)]
    pub fn new(id: String) -> napi::Result<Self> {
        Ok(Self::from(ruma::DeviceKeyId::parse(id.as_str()).map_err(into_err)?))
    }

    /// Returns key algorithm of the device key ID.
    #[napi(getter)]
    pub fn algorithm(&self) -> DeviceKeyAlgorithm {
        self.inner.algorithm().into()
    }

    /// Returns device ID of the device key ID.
    #[napi(getter)]
    pub fn device_id(&self) -> DeviceId {
        self.inner.device_id().to_owned().into()
    }

    /// Return the device key ID as a string.
    #[napi]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        self.inner.to_string()
    }
}

/// The basic key algorithms in the specification.
#[napi]
pub struct DeviceKeyAlgorithm {
    inner: ruma::DeviceKeyAlgorithm,
}

impl From<ruma::DeviceKeyAlgorithm> for DeviceKeyAlgorithm {
    fn from(inner: ruma::DeviceKeyAlgorithm) -> Self {
        Self { inner }
    }
}

#[napi]
impl DeviceKeyAlgorithm {
    /// Read the device key algorithm's name. If the name is
    /// `Unknown`, one may be interested by the `to_string` method to
    /// read the original name.
    #[napi(getter)]
    pub fn name(&self) -> DeviceKeyAlgorithmName {
        self.inner.clone().into()
    }

    /// Return the device key algorithm as a string.
    #[napi]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        self.inner.to_string()
    }
}

/// The basic key algorithm names in the specification.
#[napi]
pub enum DeviceKeyAlgorithmName {
    /// The Ed25519 signature algorithm.
    Ed25519,

    /// The Curve25519 ECDH algorithm.
    Curve25519,

    /// The Curve25519 ECDH algorithm, but the key also contains
    /// signatures.
    SignedCurve25519,

    /// An unknown device key algorithm.
    Unknown,
}

impl From<ruma::DeviceKeyAlgorithm> for DeviceKeyAlgorithmName {
    fn from(value: ruma::DeviceKeyAlgorithm) -> Self {
        use ruma::DeviceKeyAlgorithm::*;

        match value {
            Ed25519 => Self::Ed25519,
            Curve25519 => Self::Curve25519,
            SignedCurve25519 => Self::SignedCurve25519,
            _ => Self::Unknown,
        }
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

impl From<ruma::OwnedRoomId> for RoomId {
    fn from(inner: ruma::OwnedRoomId) -> Self {
        Self { inner }
    }
}

#[napi]
impl RoomId {
    /// Parse/validate and create a new `RoomId`.
    #[napi(constructor, strict)]
    pub fn new(id: String) -> napi::Result<Self> {
        Ok(Self::from(ruma::RoomId::parse(id).map_err(into_err)?))
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
    #[napi(constructor, strict)]
    pub fn new(name: String) -> napi::Result<Self> {
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
