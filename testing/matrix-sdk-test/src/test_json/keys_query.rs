// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! Templates for simulating responses to
//! `POST /_matrix/client/v3/keys/query`
//! requests.

use std::{collections::HashMap, iter};

use ruma::{
    DeviceId, UserId, api::client::keys::get_keys::v3::Response as KeyQueryResponse, device_id,
    user_id,
};
use serde_json::json;

use crate::ruma_response_from_json;

pub struct KeysQueryUser {
    pub user_id: &'static UserId,
    pub device_id: &'static DeviceId,
    device_key_curve25519: &'static str,
    device_key_ed22519: &'static str,
    device_signature: &'static str,
    device_signature_2_name: Option<&'static str>,
    device_signature_2_signature: Option<&'static str>,
    master_key_name: Option<&'static str>,
    master_key_signature: Option<&'static str>,
    master_key_device_signature: Option<&'static str>,
    self_signing_key_name: Option<&'static str>,
    self_signing_key_signature: Option<&'static str>,
}

impl KeysQueryUser {
    pub(crate) fn bob_a() -> Self {
        Self {
            user_id: user_id!("@bob:localhost"),
            device_id: device_id!("GYKSNAWLVK"),
            device_key_curve25519: "dBcZBzQaiQYWf6rBPh2QypIOB/dxSoTeyaFaxNNbeHs",
            device_key_ed22519: "6melQNnhoI9sT2b4VzNPAwa8aB179ym45fON8Yo7kVk",
            device_signature: "Fk45zHAbrd+1j9wZXLjL2Y/+DU/Mnz9yuvlfYBOOT7qExN2Jdud+5BAuNs8nZ/caS4wTF39Kg3zQpzaGERoCBg",
            device_signature_2_name: Some("dO4gmBNW7WC0bXBK81j8uh4me6085fP+keoOm0pH3gw"),
            device_signature_2_signature: Some(
                "md0Pa1MYlneFb1fp6KCsvZpi2ySb6/G+ULoCbQDWBeDxNEcoNMzf7PEKY04UToCZKUU4LifvRWmiWFDanOlkCQ",
            ),
            master_key_name: Some("/mULSzYNTdHJOBWnBmsvDHhqdHQcWnXRHHmqwzwC7DY"),
            master_key_signature: Some(
                "6vGDbPO5XzlcwbU3aV+kcck+iHHEBtX85ow2gW5U05/DZdtda/JNVa5Nn7B9lQHNnnrMqt1sX00y/JrIkSS1Aw",
            ),
            master_key_device_signature: Some(
                "jLxmUPr0Ny2Ai9+NGKGhed9BAuKikOc7r6gr7MQVawePYS95w8NJ8Tzaq9zFFOmIiojACNdQ/ksy3QAdwD6vBQ",
            ),
            self_signing_key_name: Some("dO4gmBNW7WC0bXBK81j8uh4me6085fP+keoOm0pH3gw"),
            self_signing_key_signature: Some(
                "7md6mwjUK8zjintmffJ0+kImC59/Y8PdySy99EZz5Neu+VMX3LT7txhKO2gC/hmDduRw+JGfGXIiDxR7GmQqDw",
            ),
        }
    }

    pub(crate) fn bob_b() -> Self {
        Self {
            user_id: user_id!("@bob:localhost"),
            device_id: device_id!("ATWKQFSFRN"),
            device_key_curve25519: "CY0TWVK1/Kj3ZADuBcGe3UKvpT+IKAPMUsMeJhSDqno",
            device_key_ed22519: "TyTQqd6j2JlWZh97r+kTYuCbvqnPoNwO6EGovYsjY00",
            device_signature: "BQ9Gp0p+6srF+c8OyruqKKd9R4yaub3THYAyyBB/7X/rG8BwcAqFynzl1aGyFYun4Q+087a5OSiglCXI+/kQAA",
            device_signature_2_name: Some("At1ai1VUZrCncCI7V7fEAJmBShfpqZ30xRzqcEjTjdc"),
            device_signature_2_signature: Some(
                "TWmDPaG7t0rZ6luauonELD3dmBDTIRryqXhgsIQRiGint2rJdic8RVyZ6a61bgu6mtBjfvU3prqMNp6sVi16Cg",
            ),
            master_key_name: Some("NmI78hY54kE7OZsIjbRE/iCox59t4nzScCNEO6fvtY4"),
            master_key_signature: Some(
                "xqLhC3sIUci1W2CNVW7HZWXreQApgjv2RDwB0WPiMd1P4vbZ/qJM0KWqK2piGPWliPi8YVREMrg216KXM3IhCA",
            ),
            master_key_device_signature: Some(
                "MBOzCKYPQLQMpBY2lFZJ4c8451xJfQCdhPBb1AHlTUSxKFiWi6V+k1oRRnhQein/PjkIY7ZO+HoOrIeOtbRMAw",
            ),
            self_signing_key_name: Some("At1ai1VUZrCncCI7V7fEAJmBShfpqZ30xRzqcEjTjdc"),
            self_signing_key_signature: Some(
                "Ls6CeoA4LoPCHuSwG96kbhd1dEV09TgdMROIZi6vFz/MT9Wtik6joQi/tQ3zCwIZCSR53ksLO4jG1DD31AiBAA",
            ),
        }
    }

    pub(crate) fn bob_c() -> Self {
        Self {
            user_id: user_id!("@bob:localhost"),
            device_id: device_id!("OPABMDDXGX"),
            device_key_curve25519: "O6bwa9Op0E+PQPCrbTOfdYwU+j95RRPhXIHuNpe94ns",
            device_key_ed22519: "DvjkSNOM9XrR1gWrr2YSDvTnwnLIgKDMRr5v8HgMKak",
            device_signature: "o+BBnw/SIJWxSf799Adq6jEl9X3lwCg5MJkS8GlfId+pW3ReEETK0l+9bhCAgBsNSKRtB/fmZQBhjMx4FJr+BA",
            device_signature_2_name: None,
            device_signature_2_signature: None,
            master_key_name: None,
            master_key_signature: None,
            master_key_device_signature: None,
            self_signing_key_name: None,
            self_signing_key_signature: None,
        }
    }
}

pub fn keys_query(user: &KeysQueryUser, signed_device_users: &[KeysQueryUser]) -> KeyQueryResponse {
    let device_keys = iter::once((user.user_id, device_keys(user, signed_device_users)));

    let data = json!({
        "device_keys": to_object(device_keys),
        "failures": {},
        "master_keys": master_keys(user),
        "self_signing_keys": self_signing_keys(user),
        "user_signing_keys": {}
    });
    ruma_response_from_json(&data)
}

pub fn self_signing_keys(user: &KeysQueryUser) -> serde_json::Value {
    if let (Some(self_signing_key_name), Some(self_signing_key_signature)) =
        (user.self_signing_key_name, user.self_signing_key_signature)
    {
        let master_key_name = user
            .master_key_name
            .expect("Missing master key name when we have self_signing_key_name");

        json!({
            user.user_id: {
                "keys": {
                    &format!("ed25519:{self_signing_key_name}"): self_signing_key_name
                },
                "signatures": {
                    "@bob:localhost": {
                        &format!("ed25519:{master_key_name}"): self_signing_key_signature,
                    }
                },
                "usage": [ "self_signing" ],
                "user_id": "@bob:localhost"
            }
        })
    } else {
        json!({})
    }
}

pub fn master_keys(user: &KeysQueryUser) -> serde_json::Value {
    if let (Some(master_key_name), Some(master_key_signature), Some(master_key_device_signature)) =
        (user.master_key_name, user.master_key_signature, user.master_key_device_signature)
    {
        json!({
            user.user_id: {
                "keys": { &format!("ed25519:{master_key_name}"): master_key_name },
                "signatures": {
                    user.user_id: {
                        &format!("ed25519:{master_key_name}"): master_key_signature,
                        &format!("ed25519:{}", user.device_id): master_key_device_signature
                    }
                },
                "usage": [ "master" ],
                "user_id": user.user_id
            }
        })
    } else {
        json!({})
    }
}

pub fn device_keys_payload(user: &KeysQueryUser) -> serde_json::Value {
    let mut signatures = HashMap::new();
    signatures.insert(format!("ed25519:{}", user.device_id), user.device_signature);

    if let (Some(device_signature_2_name), Some(device_signature_2_signature)) =
        (user.device_signature_2_name, user.device_signature_2_signature)
    {
        signatures
            .insert(format!("ed25519:{device_signature_2_name}"), device_signature_2_signature);
    }

    json!({
        "algorithms": [
            "m.olm.v1.curve25519-aes-sha2",
            "m.megolm.v1.aes-sha2"
        ],
        "device_id": user.device_id,
        "keys": {
            &format!("curve25519:{}", user.device_id): user.device_key_curve25519,
            &format!("ed25519:{}", user.device_id): user.device_key_ed22519,
        },
        "signatures": {
            user.user_id: signatures
        },
        "user_id": user.user_id,
    })
}

fn device_keys(user: &KeysQueryUser, signed_device_users: &[KeysQueryUser]) -> serde_json::Value {
    let mut ret = HashMap::new();

    ret.insert(user.device_id, device_keys_payload(user));

    for other in signed_device_users {
        ret.insert(other.device_id, device_keys_payload(other));
    }

    json!(ret)
}

fn to_object(
    items: impl Iterator<Item = (&'static UserId, serde_json::Value)>,
) -> serde_json::Value {
    let mp: HashMap<&'static UserId, serde_json::Value> = items.collect();
    serde_json::to_value(mp).unwrap()
}
