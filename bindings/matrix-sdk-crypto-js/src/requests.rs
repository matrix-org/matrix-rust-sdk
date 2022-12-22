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
use ruma::api::client::keys::{
    claim_keys::v3::Request as OriginalKeysClaimRequest,
    upload_keys::v3::Request as OriginalKeysUploadRequest,
    upload_signatures::v3::Request as OriginalSignatureUploadRequest,
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

    /// A JSON-encoded string representing identity keys for the device.
    ///
    /// May be absent if no new identity keys are required.
    #[wasm_bindgen(readonly)]
    pub device_keys: JsString,

    /// A JSON-encoded string representing one-time public keys for “pre-key”
    /// messages.
    #[wasm_bindgen(readonly)]
    pub one_time_keys: JsString,

    /// A JSON-encoded string representing fallback public keys for “pre-key”
    /// messages.
    #[wasm_bindgen(readonly)]
    pub fallback_keys: JsString,
}

#[wasm_bindgen]
impl KeysUploadRequest {
    /// Create a new `KeysUploadRequest`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        id: JsString,
        device_keys: JsString,
        one_time_keys: JsString,
        fallback_keys: JsString,
    ) -> KeysUploadRequest {
        Self { id: Some(id), device_keys, one_time_keys, fallback_keys }
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

    /// An optional integer representing the time (in milliseconds) to wait when
    /// downloading keys from remote servers. 10 seconds is the recommended
    /// default.
    #[wasm_bindgen(readonly)]
    pub timeout: Option<u64>,

    /// A JSON-encoded string representing the keys to be downloaded. An empty
    /// list indicates all devices for the corresponding user.
    #[wasm_bindgen(readonly)]
    pub device_keys: JsString,

    /// An optional string representing if the client is fetching keys as a
    /// result of a device update received in a sync request, this should be the
    /// `since` token of that sync request, or any later sync token. This allows
    /// the server to ensure its response contains the keys advertised by the
    /// notification in that sync.
    #[wasm_bindgen(readonly)]
    pub token: Option<String>,
}

#[wasm_bindgen]
impl KeysQueryRequest {
    /// Create a new `KeysQueryRequest`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        id: JsString,
        timeout: Option<u64>,
        device_keys: JsString,
        token: Option<String>,
    ) -> KeysQueryRequest {
        Self { id: Some(id), timeout, device_keys, token }
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

    /// An optional integer representing the time (in milliseconds) to wait when
    /// downloading keys from remote servers. 10 seconds is the recommended
    /// default.
    #[wasm_bindgen(readonly)]
    pub timeout: Option<u64>,

    /// A JSON-encoded string representing the keys to be claimed.
    #[wasm_bindgen(readonly)]
    pub one_time_keys: JsString,
}

#[wasm_bindgen]
impl KeysClaimRequest {
    /// Create a new `KeysClaimRequest`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: JsString, timeout: Option<u64>, one_time_keys: JsString) -> KeysClaimRequest {
        Self { id: Some(id), timeout, one_time_keys }
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

    /// A JSON-encoded string representing a map of users to devices to a
    /// content for a message event to be sent to the user's device. Individual
    /// message events can be sent to devices, but all events must be of the
    /// same type.
    #[wasm_bindgen(readonly)]
    pub messages: JsString,
}

#[wasm_bindgen]
impl ToDeviceRequest {
    /// Create a new `ToDeviceRequest`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        id: JsString,
        event_type: JsString,
        txn_id: JsString,
        messages: JsString,
    ) -> ToDeviceRequest {
        Self { id: Some(id), event_type, txn_id, messages }
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

    /// A JSON-encoded string representing the signed keys.
    #[wasm_bindgen(readonly)]
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

    /// A JSON-encoded string representing the event content to send.
    #[wasm_bindgen(readonly)]
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
        content: JsString,
    ) -> RoomMessageRequest {
        Self { id: Some(id), room_id, txn_id, content }
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

    /// A JSON-encoded string representing the map from room ID to a backed up
    /// room key that we're going to upload to the server.
    #[wasm_bindgen(readonly)]
    pub rooms: JsString,
}

#[wasm_bindgen]
impl KeysBackupRequest {
    /// Create a new `KeysBackupRequest`.
    #[wasm_bindgen(constructor)]
    pub fn new(id: JsString, rooms: JsString) -> KeysBackupRequest {
        Self { id: Some(id), rooms }
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

    /// A JSON-encoded string representing the user's master key..
    #[wasm_bindgen(readonly)]
    pub master_key: JsString,

    /// A JSON-encoded string representing the user's self-signing key. Must
    /// be signed with the accompanied master, or by the user's most recently
    /// uploaded master key if no master key is included in the request.
    #[wasm_bindgen(readonly)]
    pub self_signing_key: JsString,

    /// A JSON-encoded string representing the user's user-signing key. Must
    /// be signed with the accompanied master, or by the user's most recently
    /// uploaded master key if no master key is included in the request.
    #[wasm_bindgen(readonly)]
    pub user_signing_key: JsString,
}

macro_rules! request {
    ( $destination_request:ident from $source_request:ident maps fields $( $field:ident: $field_type:ident ),+ $(,)? ) => {
        impl TryFrom<&$source_request> for $destination_request {
            type Error = serde_json::Error;

            fn try_from(request: &$source_request) -> Result<Self, Self::Error> {
                request!(@__try_from $destination_request from $source_request (request_id = None, request = request) maps fields $( $field: $field_type, )+ )
            }
        }

        impl TryFrom<(String, &$source_request)> for $destination_request {
            type Error = serde_json::Error;

            fn try_from(
                (request_id, request): (String, &$source_request),
            ) -> Result<Self, Self::Error> {
                request!(@__try_from $destination_request from $source_request (request_id = Some(request_id.into()), request = request) maps fields $( $field: $field_type, )+ )
            }
        }
    };

    ( @__try_from $destination_request:ident from $source_request:ident (request_id = $request_id:expr, request = $request:expr) maps fields $( $field:ident: $field_type:ident ),+ $(,)? ) => {
        {
            Ok($destination_request {
                id: $request_id,
                $(
                    $field: request!(@__field as $field_type (request = $request, field = $field)),
                )+
            })
        }
    };

    ( @__field as json (request = $request:expr, field = $field:ident) ) => {
        serde_json::to_string(&$request.$field)?.into()
    };

    ( @__field as string (request = $request:expr, field = $field:ident) ) => {
        $request.$field.to_string().into()
    };

    ( @__field as duration (request = $request:expr, field = $field:ident) ) => {
        $request.$field.map(|duration| duration.as_millis() as u64)
    };

    ( @__field as native (request = $request:expr, field = $field:ident) ) => {
        $request.$field.clone()
    };
}

// Outgoing Requests
request!(KeysUploadRequest from OriginalKeysUploadRequest maps fields device_keys: json, one_time_keys: json, fallback_keys: json);
request!(KeysQueryRequest from OriginalKeysQueryRequest maps fields timeout: duration, device_keys: json, token: native);
request!(KeysClaimRequest from OriginalKeysClaimRequest maps fields timeout: duration, one_time_keys: json);
request!(ToDeviceRequest from OriginalToDeviceRequest maps fields event_type: string, txn_id: string, messages: json);
request!(SignatureUploadRequest from OriginalSignatureUploadRequest maps fields signed_keys: json);
request!(RoomMessageRequest from OriginalRoomMessageRequest maps fields room_id: string, txn_id: string, content: json);
request!(KeysBackupRequest from OriginalKeysBackupRequest maps fields rooms: json);

// Other Requests
request!(SigningKeysUploadRequest from OriginalUploadSigningKeysRequest maps fields master_key: json, self_signing_key: json, user_signing_key: json);

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
}
