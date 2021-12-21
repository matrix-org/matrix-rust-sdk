use serde::{Serialize, Deserialize};
use serde_json::{json, Error};
use napi_derive::napi;
use ruma::{events::{EventContent}};
use napi::bindgen_prelude::ToNapiValue;
use matrix_sdk_crypto::{OutgoingRequest};
use matrix_sdk_crypto::OutgoingRequests::*;

#[napi]
#[derive(Serialize, Deserialize)]
pub enum RequestKind {
    KeysUpload = 1,
    KeysQuery = 2,
    ToDevice = 3,
    SignatureUpload = 4,
    RoomMessage = 5,
    KeysClaim = 6,
    KeysBackup = 7,
}

pub fn outgoing_req_to_json(r: OutgoingRequest) -> Result<String, Error> {
    match r.request() {
        KeysUpload(u) => {
            serde_json::to_string(&json!({
                "request_kind": RequestKind::KeysUpload,
                "request_id": r.request_id().to_string(),
                "body": json!({
                    "device_keys": u.device_keys,
                    "one_time_keys": u.one_time_keys,
                }),
            }))
        }
        KeysQuery(k) => {
            serde_json::to_string(&json!({
                "request_kind": RequestKind::KeysQuery,
                "request_id": r.request_id().to_string(),
                "users": k.device_keys.keys().map(|u| u.to_string()).collect::<String>(),
            }))
        }
        ToDeviceRequest(t) => {
            serde_json::to_string(&json!({
                "request_kind": RequestKind::ToDevice,
                "request_id": t.txn_id_string(),
                "event_type": t.event_type.to_string(),
                "body": &t.messages,
            }))
        }
        SignatureUpload(s) => {
            serde_json::to_string(&json!({
                "request_kind": RequestKind::SignatureUpload,
                "request_id": r.request_id().to_string(),
                "body": &s.signed_keys,
            }))
        }
        RoomMessage(m) => {
            serde_json::to_string(&json!({
                "request_kind": RequestKind::RoomMessage,
                "request_id": m.txn_id.to_string(),
                "room_id": m.room_id.to_string(),
                "event_type": m.content.event_type().to_string(),
                "content": m.content,
            }))
        }
        KeysClaim(c) => {
            serde_json::to_string(&json!({
                "request_kind": RequestKind::KeysClaim,
                "request_id": r.request_id().to_string(),
                "one_time_keys": c.one_time_keys,
                // "one_time_keys": otks
                //     .into_iter()
                //     .map(|(u, d)| {
                //         (
                //             u.to_string(),
                //             d.into_iter()
                //                 .map(|(i, a)| (i.to_string(), a.to_string()))
                //                 .collect(),
                //         )
                //     })
                //     .collect(),
            }))
        }
        KeysBackup(b) => {
            serde_json::to_string(&json!({
                "request_kind": RequestKind::KeysBackup,
                "request_id": r.request_id().to_string(),
                "version": b.version.to_string(),
                "rooms": b.rooms,
            }))
        }
    }
}
