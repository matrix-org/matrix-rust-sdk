use js_sys::JsString;
use serde_json::json;
use wasm_bindgen::prelude::*;

use crate::{OutgoingRequest, OutgoingRequests};

/// Data for a request to the `upload_keys` API endpoint.
///
/// Publishes end-to-end encryption keys for the device.
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct KeysUploadRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub request_id: JsString,

    /// A JSON-encoded object of form:
    ///
    /// ```json
    /// {"device_keys": …, "one_time_keys": …}
    /// ```
    #[wasm_bindgen(readonly)]
    pub body: JsString,
}

/// Data for a request to the `get_keys` API endpoint.
///
/// Returns the current devices and identity keys for the given users.
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct KeysQueryRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub request_id: JsString,

    /// A JSON-encoded object of form:
    ///
    /// ```
    /// {"timeout": …, "device_keys": …, "token": …}
    /// ```
    #[wasm_bindgen(readonly)]
    pub body: JsString,
}

/// Data for a request to the `claim_keys` API endpoint.
///
/// Claims one-time keys for use in pre-key messages.
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct KeysClaimRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub request_id: JsString,

    /// A JSON-encoded object of form:
    ///
    /// ```
    /// {"timeout": …, "one_time_keys": …}
    /// ```
    #[wasm_bindgen(readonly)]
    pub body: JsString,
}

/// Data for a request to the `send_event_to_device` API endpoint.
///
/// Send an event to a device or devices.
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct ToDeviceRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub request_id: JsString,

    /// A JSON-encoded object of form:
    ///
    /// ```
    /// {"event_type": …, "txn_id": …, "messages": …}
    /// ```
    #[wasm_bindgen(readonly)]
    pub body: JsString,
}

/// Data for a request to the `upload_signatures` API endpoint.
///
/// Publishes cross-signing signatures for the user.
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct SignatureUploadRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub request_id: JsString,

    /// A JSON-encoded object of form:
    ///
    /// ```
    /// {"signed_keys": …, "txn_id": …, "messages": …}
    /// ```
    #[wasm_bindgen(readonly)]
    pub body: JsString,
}

/// A customized owned request type for sending out room messages.
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct RoomMessageRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub request_id: JsString,

    /// A JSON-encoded object of form:
    ///
    /// ```
    /// {"room_id": …, "txn_id": …, "content": …}
    /// ```
    #[wasm_bindgen(readonly)]
    pub body: JsString,
}

/// A request that will back up a batch of room keys to the server.
#[derive(Debug)]
#[wasm_bindgen(getter_with_clone)]
pub struct KeysBackupRequest {
    /// The request ID.
    #[wasm_bindgen(readonly)]
    pub request_id: JsString,

    /// A JSON-encoded object of form:
    ///
    /// ```
    /// {"version": …, "rooms": …}
    /// ```
    #[wasm_bindgen(readonly)]
    pub body: JsString,
}

// JavaScript has no complex enums like Rust. To return structs of
// different types, we have no choice that hidding everything behind a
// `JsValue`.
impl TryFrom<OutgoingRequest> for JsValue {
    type Error = serde_json::Error;

    fn try_from(outgoing_request: OutgoingRequest) -> Result<Self, Self::Error> {
        let request_id: JsString = outgoing_request.request_id().to_string().into();

        Ok(match outgoing_request.request() {
            OutgoingRequests::KeysUpload(request) => {
                let body = json!({
                    "device_keys": request.device_keys,
                    "one_time_keys": request.one_time_keys,
                });

                JsValue::from(KeysUploadRequest {
                    request_id,
                    body: serde_json::to_string(&body)?.into(),
                })
            }

            OutgoingRequests::KeysQuery(request) => {
                let body = json!({
                    "timeout": request.timeout,
                    "device_keys": request.device_keys,
                    "token": request.token,
                });

                JsValue::from(KeysQueryRequest {
                    request_id,
                    body: serde_json::to_string(&body)?.into(),
                })
            }

            OutgoingRequests::KeysClaim(request) => {
                let body = json!({
                    "timeout": request.timeout,
                    "one_time_keys": request.one_time_keys,
                });

                JsValue::from(KeysClaimRequest {
                    request_id,
                    body: serde_json::to_string(&body)?.into(),
                })
            }

            OutgoingRequests::ToDeviceRequest(request) => {
                let body = json!({
                    "event_type": request.event_type,
                    "txn_id": request.txn_id,
                    "messages": request.messages,
                });

                JsValue::from(ToDeviceRequest {
                    request_id,
                    body: serde_json::to_string(&body)?.into(),
                })
            }

            OutgoingRequests::SignatureUpload(request) => {
                let body = json!({
                    "signed_keys": request.signed_keys,
                });

                JsValue::from(SignatureUploadRequest {
                    request_id,
                    body: serde_json::to_string(&body)?.into(),
                })
            }

            OutgoingRequests::RoomMessage(request) => {
                let body = json!({
                    "room_id": request.room_id,
                    "txn_id": request.txn_id,
                    "content": request.content,
                });

                JsValue::from(RoomMessageRequest {
                    request_id,
                    body: serde_json::to_string(&body)?.into(),
                })
            }

            OutgoingRequests::KeysBackup(request) => {
                let body = json!({
                    "version": request.version,
                    "rooms": request.rooms,
                });

                JsValue::from(KeysBackupRequest {
                    request_id,
                    body: serde_json::to_string(&body)?.into(),
                })
            }
        })
    }
}

/// Represent the type of a request.
#[wasm_bindgen]
#[derive(Debug)]
pub enum RequestType {
    KeysUpload,
    KeysQuery,
    KeysClaim,
    ToDevice,
    SignatureUpload,
    RoomMessage,
    KeysBackup,
}
