//! Types to handle requests.

use js_sys::JsString;
use matrix_sdk_crypto::{
    requests::{
        KeysBackupRequest as OriginalKeysBackupRequest,
        KeysQueryRequest as OriginalKeysQueryRequest,
        RoomMessageRequest as OriginalRoomMessageRequest,
        ToDeviceRequest as OriginalToDeviceRequest,
        UploadSigningKeysRequest as OriginalUploadSigningKeysRequest,
    },
    OutgoingRequests,
};
use ruma::{
    api::client::keys::{
        claim_keys::v3::Request as OriginalKeysClaimRequest,
        upload_keys::v3::Request as OriginalKeysUploadRequest,
        upload_signatures::v3::Request as OriginalSignatureUploadRequest,
    },
    events::EventContent,
};
use wasm_bindgen::prelude::*;

/** Outgoing Requests * */

/// Data for a request to the `/keys/upload` API endpoint
/// ([specification]).
///
/// Publishes end-to-end encryption keys for the device.
///
/// [specification]: https://spec.matrix.org/unstable/client-server-api/#post_matrixclientv3keysupload
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct KeysUploadRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub id: Option<JsString>,

    /// A JSON-encoded string containing the rest of the payload: `device_keys`,
    /// `one_time_keys`, `fallback_keys`.
    ///
    /// It represents the body of the HTTP request.
    #[wasm_bindgen(readonly)]
    pub body: JsString,
}

#[wasm_bindgen]
impl KeysUploadRequest {
    /// Create a new `KeysUploadRequest`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: JsString, body: JsString) -> KeysUploadRequest {
        Self { id: Some(id), body }
    }

    /// Get its request type.
    #[wasm_bindgen(getter, js_name = "type")]
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
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct KeysQueryRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub id: Option<JsString>,

    /// A JSON-encoded string containing the rest of the payload: `timeout`,
    /// `device_keys`, `token`.
    ///
    /// It represents the body of the HTTP request.
    #[wasm_bindgen(readonly)]
    pub body: JsString,
}

#[wasm_bindgen]
impl KeysQueryRequest {
    /// Create a new `KeysQueryRequest`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: JsString, body: JsString) -> KeysQueryRequest {
        Self { id: Some(id), body }
    }

    /// Get its request type.
    #[wasm_bindgen(getter, js_name = "type")]
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
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct KeysClaimRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub id: Option<JsString>,

    /// A JSON-encoded string containing the rest of the payload: `timeout`,
    /// `one_time_keys`.
    ///
    /// It represents the body of the HTTP request.
    #[wasm_bindgen(readonly)]
    pub body: JsString,
}

#[wasm_bindgen]
impl KeysClaimRequest {
    /// Create a new `KeysClaimRequest`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: JsString, body: JsString) -> KeysClaimRequest {
        Self { id: Some(id), body }
    }

    /// Get its request type.
    #[wasm_bindgen(getter, js_name = "type")]
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
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct ToDeviceRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub id: Option<JsString>,

    /// A string representing the type of event being sent to each devices.
    #[wasm_bindgen(readonly)]
    pub event_type: JsString,

    /// A string representing a request identifier unique to the access token
    /// used to send the request.
    #[wasm_bindgen(readonly)]
    pub txn_id: JsString,

    /// A JSON-encoded string containing the rest of the payload: `messages`.
    ///
    /// It represents the body of the HTTP request.
    #[wasm_bindgen(readonly)]
    pub body: JsString,
}

#[wasm_bindgen]
impl ToDeviceRequest {
    /// Create a new `ToDeviceRequest`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        id: JsString,
        event_type: JsString,
        txn_id: JsString,
        body: JsString,
    ) -> ToDeviceRequest {
        Self { id: Some(id), event_type, txn_id, body }
    }

    /// Get its request type.
    #[wasm_bindgen(getter, js_name = "type")]
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
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct SignatureUploadRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub id: Option<JsString>,

    /// A JSON-encoded string containing the payload of the request
    ///
    /// It represents the body of the HTTP request.
    #[wasm_bindgen(readonly, js_name = "body")]
    pub signed_keys: JsString,
}

#[wasm_bindgen]
impl SignatureUploadRequest {
    /// Create a new `SignatureUploadRequest`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: JsString, signed_keys: JsString) -> SignatureUploadRequest {
        Self { id: Some(id), signed_keys }
    }

    /// Get its request type.
    #[wasm_bindgen(getter, js_name = "type")]
    pub fn request_type(&self) -> RequestType {
        RequestType::SignatureUpload
    }
}

/// A customized owned request type for sending out room messages
/// ([specification]).
///
/// [specification]: https://spec.matrix.org/unstable/client-server-api/#put_matrixclientv3roomsroomidsendeventtypetxnid
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct RoomMessageRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub id: Option<JsString>,

    /// A string representing the room to send the event to.
    #[wasm_bindgen(readonly)]
    pub room_id: JsString,

    /// A string representing the transaction ID for this event.
    ///
    /// Clients should generate an ID unique across requests with the same
    /// access token; it will be used by the server to ensure idempotency of
    /// requests.
    #[wasm_bindgen(readonly)]
    pub txn_id: JsString,

    /// A string representing the type of event to be sent.
    #[wasm_bindgen(readonly)]
    pub event_type: JsString,

    /// A JSON-encoded string containing the message's content.
    #[wasm_bindgen(readonly, js_name = "body")]
    pub content: JsString,
}

#[wasm_bindgen]
impl RoomMessageRequest {
    /// Create a new `RoomMessageRequest`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        id: JsString,
        room_id: JsString,
        txn_id: JsString,
        event_type: JsString,
        content: JsString,
    ) -> RoomMessageRequest {
        Self { id: Some(id), room_id, txn_id, event_type, content }
    }

    /// Get its request type.
    #[wasm_bindgen(getter, js_name = "type")]
    pub fn request_type(&self) -> RequestType {
        RequestType::RoomMessage
    }
}

/// A request that will back up a batch of room keys to the server
/// ([specification]).
///
/// [specification]: https://spec.matrix.org/unstable/client-server-api/#put_matrixclientv3room_keyskeys
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct KeysBackupRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub id: Option<JsString>,

    /// A JSON-encoded string containing the rest of the payload: `rooms`.
    ///
    /// It represents the body of the HTTP request.
    #[wasm_bindgen(readonly)]
    pub body: JsString,
}

#[wasm_bindgen]
impl KeysBackupRequest {
    /// Create a new `KeysBackupRequest`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: JsString, body: JsString) -> KeysBackupRequest {
        Self { id: Some(id), body }
    }

    /// Get its request type.
    #[wasm_bindgen(getter, js_name = "type")]
    pub fn request_type(&self) -> RequestType {
        RequestType::KeysBackup
    }
}

/** Other Requests * */

/// Request that will publish a cross signing identity.
///
/// This uploads the public cross signing key triplet.
#[wasm_bindgen(getter_with_clone)]
#[derive(Debug)]
pub struct SigningKeysUploadRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub id: Option<JsString>,

    /// A JSON-encoded string containing the rest of the payload: `master_key`,
    /// `self_signing_key`, `user_signing_key`.
    ///
    /// It represents the body of the HTTP request.
    #[wasm_bindgen(readonly)]
    pub body: JsString,
}

#[wasm_bindgen]
impl SigningKeysUploadRequest {
    /// Create a new `SigningKeysUploadRequest`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: JsString, body: JsString) -> SigningKeysUploadRequest {
        Self { id: Some(id), body }
    }

    /// Get its request type.
    #[wasm_bindgen(getter, js_name = "type")]
    pub fn request_type(&self) -> RequestType {
        RequestType::SigningKeysUpload
    }
}

macro_rules! request {
    (
        $destination_request:ident from $source_request:ident
        $( extracts $( $field_name:ident : $field_type:tt ),+ $(,)? )?
        $( $( and )? groups $( $grouped_field_name:ident ),+ $(,)? )?
    ) => {
        impl TryFrom<&$source_request> for $destination_request {
            type Error = serde_json::Error;

            fn try_from(request: &$source_request) -> Result<Self, Self::Error> {
                request!(
                    @__try_from $destination_request from $source_request
                    (request_id = None, request = request)
                    $( extracts [ $( $field_name : $field_type, )+ ] )?
                    $( groups [ $( $grouped_field_name, )+ ] )?
                )
            }
        }

        impl TryFrom<(String, &$source_request)> for $destination_request {
            type Error = serde_json::Error;

            fn try_from(
                (request_id, request): (String, &$source_request),
            ) -> Result<Self, Self::Error> {
                request!(
                    @__try_from $destination_request from $source_request
                    (request_id = Some(request_id.into()), request = request)
                    $( extracts [ $( $field_name : $field_type, )+ ] )?
                    $( groups [ $( $grouped_field_name, )+ ] )?
                )
            }
        }
    };

    (
        @__try_from $destination_request:ident from $source_request:ident
        (request_id = $request_id:expr, request = $request:expr)
        $( extracts [ $( $field_name:ident : $field_type:tt ),* $(,)? ] )?
        $( groups [ $( $grouped_field_name:ident ),* $(,)? ] )?
    ) => {
        {
            Ok($destination_request {
                id: $request_id,
                $(
                    $(
                        $field_name: request!(@__field $field_name : $field_type ; request = $request),
                    )*
                )?
                $(
                    body: {
                        let mut map = serde_json::Map::new();
                        $(
                            map.insert(stringify!($grouped_field_name).to_owned(), serde_json::to_value(&$request.$grouped_field_name).unwrap());
                        )*
                        let object = serde_json::Value::Object(map);

                        serde_json::to_string(&object)?.into()
                    }
                )?
            })
        }
    };

    ( @__field $field_name:ident : $field_type:ident ; request = $request:expr ) => {
        request!(@__field_type as $field_type ; request = $request, field_name = $field_name)
    };

    ( @__field_type as string ; request = $request:expr, field_name = $field_name:ident ) => {
        $request.$field_name.to_string().into()
    };

    ( @__field_type as json ; request = $request:expr, field_name = $field_name:ident ) => {
        serde_json::to_string(&$request.$field_name)?.into()
    };

    ( @__field_type as event_type ; request = $request:expr, field_name = $field_name:ident ) => {
        $request.content.event_type().to_string().into()
    };
}

// Outgoing Requests
request!(KeysUploadRequest from OriginalKeysUploadRequest groups device_keys, one_time_keys, fallback_keys);
request!(KeysQueryRequest from OriginalKeysQueryRequest groups timeout, device_keys, token);
request!(KeysClaimRequest from OriginalKeysClaimRequest groups timeout, one_time_keys);
request!(ToDeviceRequest from OriginalToDeviceRequest extracts event_type: string, txn_id: string and groups messages);
request!(SignatureUploadRequest from OriginalSignatureUploadRequest extracts signed_keys: json);
request!(RoomMessageRequest from OriginalRoomMessageRequest extracts room_id: string, txn_id: string, event_type: event_type, content: json);
request!(KeysBackupRequest from OriginalKeysBackupRequest groups rooms);

// Other Requests
request!(SigningKeysUploadRequest from OriginalUploadSigningKeysRequest groups master_key, self_signing_key, user_signing_key);

// JavaScript has no complex enums like Rust. To return structs of
// different types, we have no choice that hiding everything behind a
// `JsValue`.
pub(crate) struct OutgoingRequest(pub(crate) matrix_sdk_crypto::OutgoingRequest);

impl TryFrom<OutgoingRequest> for JsValue {
    type Error = serde_json::Error;

    fn try_from(outgoing_request: OutgoingRequest) -> Result<Self, Self::Error> {
        let request_id = outgoing_request.0.request_id().to_string();

        Ok(match outgoing_request.0.request() {
            OutgoingRequests::KeysUpload(request) => {
                JsValue::from(KeysUploadRequest::try_from((request_id, request))?)
            }

            OutgoingRequests::KeysQuery(request) => {
                JsValue::from(KeysQueryRequest::try_from((request_id, request))?)
            }

            OutgoingRequests::KeysClaim(request) => {
                JsValue::from(KeysClaimRequest::try_from((request_id, request))?)
            }

            OutgoingRequests::ToDeviceRequest(request) => {
                JsValue::from(ToDeviceRequest::try_from((request_id, request))?)
            }

            OutgoingRequests::SignatureUpload(request) => {
                JsValue::from(SignatureUploadRequest::try_from((request_id, request))?)
            }

            OutgoingRequests::RoomMessage(request) => {
                JsValue::from(RoomMessageRequest::try_from((request_id, request))?)
            }

            OutgoingRequests::KeysBackup(request) => {
                JsValue::from(KeysBackupRequest::try_from((request_id, request))?)
            }
        })
    }
}

/// Represent the type of a request.
#[wasm_bindgen]
#[derive(Debug)]
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

    /// Represents a `SigningKeysUploadRequest`
    SigningKeysUpload,
}
