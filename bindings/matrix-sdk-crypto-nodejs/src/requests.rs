//! Types to handle requests.

use std::time::Duration;

use matrix_sdk_crypto::requests::{
    KeysBackupRequest as RumaKeysBackupRequest, KeysQueryRequest as RumaKeysQueryRequest,
    RoomMessageRequest as RumaRoomMessageRequest, ToDeviceRequest as RumaToDeviceRequest,
};
use napi::bindgen_prelude::{Either7, ToNapiValue};
use napi_derive::*;
use ruma::api::client::keys::{
    claim_keys::v3::Request as RumaKeysClaimRequest,
    upload_keys::v3::Request as RumaKeysUploadRequest,
    upload_signatures::v3::Request as RumaSignatureUploadRequest,
};

/// Data for a request to the `/keys/upload` API endpoint
/// ([specification]).
///
/// Publishes end-to-end encryption keys for the device.
///
/// [specification]: https://spec.matrix.org/unstable/client-server-api/#post_matrixclientv3keysupload
#[napi]
pub struct KeysUploadRequest {
    /// The request ID.
    #[napi(readonly)]
    pub id: String,

    /// A JSON-encoded object of form:
    ///
    /// ```json
    /// {"device_keys": …, "one_time_keys": …, "fallback_keys": …}
    /// ```
    #[napi(readonly)]
    pub body: String,
}

#[napi]
impl KeysUploadRequest {
    /// Get its request type.
    #[napi(getter, js_name = "type")]
    pub fn request_type(&self) -> RequestType {
        RequestType::KeysUpload
    }
}

/// Data for a request to the `/keys/query` API endpoint
/// ([specification]).
///
/// Returns the current devices and identity keys for the given users.
///
/// [specification]: https://spec.matrix.org/unstable/client-server-api/#post_matrixclientv3keysquery
#[napi]
pub struct KeysQueryRequest {
    /// The request ID.
    #[napi(readonly)]
    pub id: String,

    /// A JSON-encoded object of form:
    ///
    /// ```json
    /// {"timeout": …, "device_keys": …, "token": …}
    /// ```
    #[napi(readonly)]
    pub body: String,
}

#[napi]
impl KeysQueryRequest {
    /// Get its request type.
    #[napi(getter, js_name = "type")]
    pub fn request_type(&self) -> RequestType {
        RequestType::KeysQuery
    }
}

/// Data for a request to the `/keys/claim` API endpoint
/// ([specification]).
///
/// Claims one-time keys that can be used to establish 1-to-1 E2EE
/// sessions.
///
/// [specification]: https://spec.matrix.org/unstable/client-server-api/#post_matrixclientv3keysclaim
#[napi]
pub struct KeysClaimRequest {
    /// The request ID.
    #[napi(readonly)]
    pub id: String,

    /// A JSON-encoded object of form:
    ///
    /// ```json
    /// {"timeout": …, "one_time_keys": …}
    /// ```
    #[napi(readonly)]
    pub body: String,
}

#[napi]
impl KeysClaimRequest {
    /// Get its request type.
    #[napi(getter, js_name = "type")]
    pub fn request_type(&self) -> RequestType {
        RequestType::KeysClaim
    }
}

/// Data for a request to the `/sendToDevice` API endpoint
/// ([specification]).
///
/// Send an event to a single device or to a group of devices.
///
/// [specification]: https://spec.matrix.org/unstable/client-server-api/#put_matrixclientv3sendtodeviceeventtypetxnid
#[napi]
pub struct ToDeviceRequest {
    /// The request ID.
    #[napi(readonly)]
    pub id: String,

    /// A JSON-encoded object of form:
    ///
    /// ```json
    /// {"event_type": …, "txn_id": …, "messages": …}
    /// ```
    #[napi(readonly)]
    pub body: String,
}

#[napi]
impl ToDeviceRequest {
    /// Get its request type.
    #[napi(getter, js_name = "type")]
    pub fn request_type(&self) -> RequestType {
        RequestType::ToDevice
    }
}

/// Data for a request to the `/keys/signatures/upload` API endpoint
/// ([specification]).
///
/// Publishes cross-signing signatures for the user.
///
/// [specification]: https://spec.matrix.org/unstable/client-server-api/#post_matrixclientv3keyssignaturesupload
#[napi]
pub struct SignatureUploadRequest {
    /// The request ID.
    #[napi(readonly)]
    pub id: String,

    /// A JSON-encoded object of form:
    ///
    /// ```json
    /// {"signed_keys": …, "txn_id": …, "messages": …}
    /// ```
    #[napi(readonly)]
    pub body: String,
}

#[napi]
impl SignatureUploadRequest {
    /// Get its request type.
    #[napi(getter, js_name = "type")]
    pub fn request_type(&self) -> RequestType {
        RequestType::SignatureUpload
    }
}

/// A customized owned request type for sending out room messages
/// ([specification]).
///
/// [specification]: https://spec.matrix.org/unstable/client-server-api/#put_matrixclientv3roomsroomidsendeventtypetxnid
#[napi]
pub struct RoomMessageRequest {
    /// The request ID.
    #[napi(readonly)]
    pub id: String,

    /// A JSON-encoded object of form:
    ///
    /// ```json
    /// {"room_id": …, "txn_id": …, "content": …}
    /// ```
    #[napi(readonly)]
    pub body: String,
}

#[napi]
impl RoomMessageRequest {
    /// Get its request type.
    #[napi(getter, js_name = "type")]
    pub fn request_type(&self) -> RequestType {
        RequestType::RoomMessage
    }
}

/// A request that will back up a batch of room keys to the server
/// ([specification]).
///
/// [specification]: https://spec.matrix.org/unstable/client-server-api/#put_matrixclientv3room_keyskeys
#[napi]
pub struct KeysBackupRequest {
    /// The request ID.
    #[napi(readonly)]
    pub id: String,

    /// A JSON-encoded object of form:
    ///
    /// ```json
    /// {"rooms": …}
    /// ```
    #[napi(readonly)]
    pub body: String,
}

#[napi]
impl KeysBackupRequest {
    /// Get its request type.
    #[napi(getter, js_name = "type")]
    pub fn request_type(&self) -> RequestType {
        RequestType::KeysBackup
    }
}

macro_rules! request {
    ($request:ident from $ruma_request:ident maps fields $( $field:ident $( { $transformation:expr } )? ),+ $(,)? ) => {
        impl TryFrom<(String, &$ruma_request)> for $request {
            type Error = serde_json::Error;

            fn try_from(
                (request_id, request): (String, &$ruma_request),
            ) -> Result<Self, Self::Error> {
                let mut map = serde_json::Map::new();
                $(
                    let field = &request.$field;
                    $(
                        let field = {
                            let $field = field;

                            $transformation
                        };
                    )?
                    map.insert(stringify!($field).to_owned(), serde_json::to_value(field)?);
                )+
                let value = serde_json::Value::Object(map);

                Ok($request {
                    id: request_id,
                    body: serde_json::to_string(&value)?.into(),
                })
            }
        }
    };
}

request!(KeysUploadRequest from RumaKeysUploadRequest maps fields device_keys, one_time_keys, fallback_keys);
request!(KeysQueryRequest from RumaKeysQueryRequest maps fields timeout { timeout.as_ref().map(Duration::as_millis) }, device_keys, token);
request!(KeysClaimRequest from RumaKeysClaimRequest maps fields timeout { timeout.as_ref().map(Duration::as_millis) }, one_time_keys);
request!(ToDeviceRequest from RumaToDeviceRequest maps fields event_type, txn_id, messages);
request!(SignatureUploadRequest from RumaSignatureUploadRequest maps fields signed_keys);
request!(RoomMessageRequest from RumaRoomMessageRequest maps fields room_id, txn_id, content);
request!(KeysBackupRequest from RumaKeysBackupRequest maps fields rooms);

pub type OutgoingRequests = Either7<
    KeysUploadRequest,
    KeysQueryRequest,
    KeysClaimRequest,
    ToDeviceRequest,
    SignatureUploadRequest,
    RoomMessageRequest,
    KeysBackupRequest,
>;

pub(crate) struct OutgoingRequest(pub(crate) matrix_sdk_crypto::OutgoingRequest);

impl TryFrom<OutgoingRequest> for OutgoingRequests {
    type Error = serde_json::Error;

    fn try_from(outgoing_request: OutgoingRequest) -> Result<Self, Self::Error> {
        let request_id = outgoing_request.0.request_id().to_string();

        Ok(match outgoing_request.0.request() {
            matrix_sdk_crypto::OutgoingRequests::KeysUpload(request) => {
                Either7::A(KeysUploadRequest::try_from((request_id, request))?)
            }

            matrix_sdk_crypto::OutgoingRequests::KeysQuery(request) => {
                Either7::B(KeysQueryRequest::try_from((request_id, request))?)
            }

            matrix_sdk_crypto::OutgoingRequests::KeysClaim(request) => {
                Either7::C(KeysClaimRequest::try_from((request_id, request))?)
            }

            matrix_sdk_crypto::OutgoingRequests::ToDeviceRequest(request) => {
                Either7::D(ToDeviceRequest::try_from((request_id, request))?)
            }

            matrix_sdk_crypto::OutgoingRequests::SignatureUpload(request) => {
                Either7::E(SignatureUploadRequest::try_from((request_id, request))?)
            }

            matrix_sdk_crypto::OutgoingRequests::RoomMessage(request) => {
                Either7::F(RoomMessageRequest::try_from((request_id, request))?)
            }

            matrix_sdk_crypto::OutgoingRequests::KeysBackup(request) => {
                Either7::G(KeysBackupRequest::try_from((request_id, request))?)
            }
        })
    }
}

/// Represent the type of a request.
#[napi]
pub enum RequestType {
    /// Represents a `KeysUploadRequest`.
    KeysUpload,

    /// Represents a `KeysQueryRequest`.
    KeysQuery,

    /// Represents a `KeysClaimRequest`.
    KeysClaim,

    /// Represents a `ToDeviceRequest`.
    ToDevice,

    /// Represents a `SignatureUploadRequest`.
    SignatureUpload,

    /// Represents a `RoomMessageRequest`.
    RoomMessage,

    /// Represents a `KeysBackupRequest`.
    KeysBackup,
}
