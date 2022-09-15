//! Types for [Matrix](https://matrix.org/) identifiers for devices,
//! events, keys, rooms, servers, users and URIs.

use wasm_bindgen::prelude::*;

use crate::impl_from_to_inner;

/// A Matrix [user ID].
///
/// [user ID]: https://spec.matrix.org/v1.2/appendices/#user-identifiers
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct UserId {
    pub(crate) inner: ruma::OwnedUserId,
}

impl_from_to_inner!(ruma::OwnedUserId => UserId);

#[wasm_bindgen]
impl UserId {
    /// Parse/validate and create a new `UserId`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: &str) -> Result<UserId, JsError> {
        Ok(Self::from(ruma::UserId::parse(id)?))
    }

    /// Returns the user's localpart.
    #[wasm_bindgen(getter)]
    pub fn localpart(&self) -> String {
        self.inner.localpart().to_owned()
    }

    /// Returns the server name of the user ID.
    #[wasm_bindgen(getter, js_name = "serverName")]
    pub fn server_name(&self) -> ServerName {
        ServerName { inner: self.inner.server_name().to_owned() }
    }

    /// Whether this user ID is a historical one.
    ///
    /// A historical user ID is one that doesn't conform to the latest
    /// specification of the user ID grammar but is still accepted
    /// because it was previously allowed.
    #[wasm_bindgen(js_name = "isHistorical")]
    pub fn is_historical(&self) -> bool {
        self.inner.is_historical()
    }

    /// Return the user ID as a string.
    #[wasm_bindgen(js_name = "toString")]
    #[allow(clippy::inherent_to_string)]
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

impl_from_to_inner!(ruma::OwnedDeviceId => DeviceId);

#[wasm_bindgen]
impl DeviceId {
    /// Create a new `DeviceId`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: &str) -> DeviceId {
        Self::from(ruma::OwnedDeviceId::from(id))
    }

    /// Return the device ID as a string.
    #[wasm_bindgen(js_name = "toString")]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        self.inner.as_str().to_owned()
    }
}

/// A Matrix device key ID.
///
/// A key algorithm and a device ID, combined with a ‘:’.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct DeviceKeyId {
    pub(crate) inner: ruma::OwnedDeviceKeyId,
}

impl_from_to_inner!(ruma::OwnedDeviceKeyId => DeviceKeyId);

#[wasm_bindgen]
impl DeviceKeyId {
    /// Parse/validate and create a new `DeviceKeyId`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: String) -> Result<DeviceKeyId, JsError> {
        Ok(Self::from(ruma::DeviceKeyId::parse(id.as_str())?))
    }

    /// Returns key algorithm of the device key ID.
    #[wasm_bindgen(getter)]
    pub fn algorithm(&self) -> DeviceKeyAlgorithm {
        self.inner.algorithm().into()
    }

    /// Returns device ID of the device key ID.
    #[wasm_bindgen(getter, js_name = "deviceId")]
    pub fn device_id(&self) -> DeviceId {
        self.inner.device_id().to_owned().into()
    }

    /// Return the device key ID as a string.
    #[wasm_bindgen(js_name = "toString")]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        self.inner.to_string()
    }
}

/// The basic key algorithms in the specification.
#[wasm_bindgen]
#[derive(Debug)]
pub struct DeviceKeyAlgorithm {
    pub(crate) inner: ruma::DeviceKeyAlgorithm,
}

impl_from_to_inner!(ruma::DeviceKeyAlgorithm => DeviceKeyAlgorithm);

#[wasm_bindgen]
impl DeviceKeyAlgorithm {
    /// Read the device key algorithm's name. If the name is
    /// `Unknown`, one may be interested by the `to_string` method to
    /// read the original name.
    #[wasm_bindgen(getter)]
    pub fn name(&self) -> DeviceKeyAlgorithmName {
        self.inner.clone().into()
    }

    /// Return the device key algorithm as a string.
    #[wasm_bindgen(js_name = "toString")]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        self.inner.to_string()
    }
}

/// The basic key algorithm names in the specification.
#[wasm_bindgen]
#[derive(Debug)]
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

impl TryFrom<DeviceKeyAlgorithmName> for ruma::DeviceKeyAlgorithm {
    type Error = JsError;

    fn try_from(value: DeviceKeyAlgorithmName) -> Result<Self, Self::Error> {
        use DeviceKeyAlgorithmName::*;

        Ok(match value {
            Ed25519 => Self::Ed25519,
            Curve25519 => Self::Curve25519,
            SignedCurve25519 => Self::SignedCurve25519,
            Unknown => {
                return Err(JsError::new(
                    "The `DeviceKeyAlgorithmName.Unknown` variant cannot be converted",
                ))
            }
        })
    }
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
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub struct RoomId {
    pub(crate) inner: ruma::OwnedRoomId,
}

impl_from_to_inner!(ruma::OwnedRoomId => RoomId);

#[wasm_bindgen]
impl RoomId {
    /// Parse/validate and create a new `RoomId`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: &str) -> Result<RoomId, JsError> {
        Ok(Self::from(ruma::RoomId::parse(id)?))
    }

    /// Returns the user's localpart.
    #[wasm_bindgen(getter)]
    pub fn localpart(&self) -> String {
        self.inner.localpart().to_owned()
    }

    /// Returns the server name of the room ID.
    #[wasm_bindgen(getter, js_name = "serverName")]
    pub fn server_name(&self) -> ServerName {
        ServerName { inner: self.inner.server_name().to_owned() }
    }

    /// Return the room ID as a string.
    #[wasm_bindgen(js_name = "toString")]
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
    #[wasm_bindgen(getter)]
    pub fn host(&self) -> String {
        self.inner.host().to_owned()
    }

    /// Returns the port of the server name if any.
    #[wasm_bindgen(getter)]
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

/// A Matrix [event ID].
///
/// An `EventId` is generated randomly or converted from a string
/// slice, and can be converted back into a string as needed.
///
/// [event ID]: https://spec.matrix.org/v1.2/appendices/#room-ids-and-event-ids
#[wasm_bindgen]
pub struct EventId {
    pub(crate) inner: ruma::OwnedEventId,
}

#[wasm_bindgen]
impl EventId {
    /// Parse/validate and create a new `EventId`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: &str) -> Result<EventId, JsError> {
        Ok(Self { inner: <&ruma::EventId>::try_from(id)?.to_owned() })
    }

    /// Returns the event's localpart.
    #[wasm_bindgen(getter)]
    pub fn localpart(&self) -> String {
        self.inner.localpart().to_owned()
    }

    /// Returns the server name of the event ID.
    #[wasm_bindgen(getter, js_name = "serverName")]
    pub fn server_name(&self) -> Option<ServerName> {
        Some(ServerName { inner: self.inner.server_name()?.to_owned() })
    }

    /// Return the event ID as a string.
    #[wasm_bindgen(js_name = "toString")]
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        self.inner.as_str().to_owned()
    }
}
