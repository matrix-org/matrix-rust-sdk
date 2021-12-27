// Copyright 2021 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use matrix_sdk_common::uuid::Uuid;
use matrix_sdk_crypto::{OutgoingRequest, OutgoingRequests::*, ToDeviceRequest};
use napi::bindgen_prelude::ToNapiValue;
use napi_derive::napi;
use ruma::{api::client::r0::keys::claim_keys::Request as KeysClaimRequest, events::EventContent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Error};

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

pub fn key_claim_to_request(tpl: (Uuid, KeysClaimRequest)) -> Result<String, Error> {
    let (request_id, request) = tpl;
    key_claim_request_serialize(&request_id, &request)
}

pub fn outgoing_req_to_json(r: OutgoingRequest) -> Result<String, Error> {
    match r.request() {
        KeysUpload(u) => serde_json::to_string(&json!({
            "request_kind": RequestKind::KeysUpload,
            "request_id": r.request_id().to_string(),
            "body": json!({
                "device_keys": u.device_keys,
                "one_time_keys": u.one_time_keys,
            }),
        })),
        KeysQuery(k) => serde_json::to_string(&json!({
            "request_kind": RequestKind::KeysQuery,
            "request_id": r.request_id().to_string(),
            "users": k.device_keys.keys().map(|u| u.to_string()).collect::<String>(),
        })),
        ToDeviceRequest(t) => to_device_request_serialize(t),
        SignatureUpload(s) => serde_json::to_string(&json!({
            "request_kind": RequestKind::SignatureUpload,
            "request_id": r.request_id().to_string(),
            "body": &s.signed_keys,
        })),
        RoomMessage(m) => serde_json::to_string(&json!({
            "request_kind": RequestKind::RoomMessage,
            "request_id": m.txn_id.to_string(),
            "room_id": m.room_id.to_string(),
            "event_type": m.content.event_type().to_string(),
            "content": m.content,
        })),
        KeysClaim(c) => key_claim_request_serialize(r.request_id(), c),
        KeysBackup(b) => serde_json::to_string(&json!({
            "request_kind": RequestKind::KeysBackup,
            "request_id": r.request_id().to_string(),
            "version": b.version.to_string(),
            "rooms": b.rooms,
        })),
    }
}

pub fn to_device_request_serialize(r: &ToDeviceRequest) -> Result<String, Error> {
    serde_json::to_string(&json!({
        "request_kind": RequestKind::ToDevice,
        "request_id": r.txn_id_string(),
        "event_type": r.event_type.to_string(),
        "body": &r.messages,
    }))
}

fn key_claim_request_serialize(id: &Uuid, r: &KeysClaimRequest) -> Result<String, Error> {
    serde_json::to_string(&json!({
        "request_kind": RequestKind::KeysClaim,
        "request_id": id.to_string(),
        "one_time_keys": r.one_time_keys,
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
