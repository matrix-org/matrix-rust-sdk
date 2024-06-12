use ruma::{
    api::{client::keys::get_keys::v3::Response as KeyQueryResponse, IncomingResponse},
    device_id, user_id, DeviceId, UserId,
};
use serde_json::{json, Value};

use crate::machine::testing::response_from_file;

pub(crate) struct IdentityChangeDataSet {}

#[allow(dead_code)]
impl IdentityChangeDataSet {
    pub fn user_id() -> &'static UserId {
        user_id!("@bob:localhost")
    }

    pub fn first_device_id() -> &'static DeviceId {
        device_id!("GYKSNAWLVK")
    }

    pub fn second_device_id() -> &'static DeviceId {
        device_id!("ATWKQFSFRN")
    }

    pub fn third_device_id() -> &'static DeviceId {
        device_id!("OPABMDDXGX")
    }

    fn device_keys_payload_1_signed_by_a() -> Value {
        json!({
            "algorithms": [
                "m.olm.v1.curve25519-aes-sha2",
                "m.megolm.v1.aes-sha2"
            ],
            "device_id": "GYKSNAWLVK",
            "keys": {
                "curve25519:GYKSNAWLVK": "dBcZBzQaiQYWf6rBPh2QypIOB/dxSoTeyaFaxNNbeHs",
                "ed25519:GYKSNAWLVK": "6melQNnhoI9sT2b4VzNPAwa8aB179ym45fON8Yo7kVk"
            },
            "signatures": {
                "@bob:localhost": {
                    "ed25519:GYKSNAWLVK": "Fk45zHAbrd+1j9wZXLjL2Y/+DU/Mnz9yuvlfYBOOT7qExN2Jdud+5BAuNs8nZ/caS4wTF39Kg3zQpzaGERoCBg",
                    "ed25519:dO4gmBNW7WC0bXBK81j8uh4me6085fP+keoOm0pH3gw": "md0Pa1MYlneFb1fp6KCsvZpi2ySb6/G+ULoCbQDWBeDxNEcoNMzf7PEKY04UToCZKUU4LifvRWmiWFDanOlkCQ"
                }
            },
            "user_id": "@bob:localhost",
        })
    }

    fn msk_a() -> Value {
        json!({
            "@bob:localhost": {
                "keys": {
                    "ed25519:/mULSzYNTdHJOBWnBmsvDHhqdHQcWnXRHHmqwzwC7DY": "/mULSzYNTdHJOBWnBmsvDHhqdHQcWnXRHHmqwzwC7DY"
                },
                "signatures": {
                    "@bob:localhost": {
                        "ed25519:/mULSzYNTdHJOBWnBmsvDHhqdHQcWnXRHHmqwzwC7DY": "6vGDbPO5XzlcwbU3aV+kcck+iHHEBtX85ow2gW5U05/DZdtda/JNVa5Nn7B9lQHNnnrMqt1sX00y/JrIkSS1Aw",
                        "ed25519:GYKSNAWLVK": "jLxmUPr0Ny2Ai9+NGKGhed9BAuKikOc7r6gr7MQVawePYS95w8NJ8Tzaq9zFFOmIiojACNdQ/ksy3QAdwD6vBQ"
                    }
                },
                "usage": [
                    "master"
                ],
                "user_id": "@bob:localhost"
            }
        })
    }
    fn ssk_a() -> Value {
        json!({
            "@bob:localhost": {
                "keys": {
                    "ed25519:dO4gmBNW7WC0bXBK81j8uh4me6085fP+keoOm0pH3gw": "dO4gmBNW7WC0bXBK81j8uh4me6085fP+keoOm0pH3gw"
                },
                "signatures": {
                    "@bob:localhost": {
                        "ed25519:/mULSzYNTdHJOBWnBmsvDHhqdHQcWnXRHHmqwzwC7DY": "7md6mwjUK8zjintmffJ0+kImC59/Y8PdySy99EZz5Neu+VMX3LT7txhKO2gC/hmDduRw+JGfGXIiDxR7GmQqDw"
                    }
                },
                "usage": [
                    "self_signing"
                ],
                "user_id": "@bob:localhost"
            }
        })
    }
    /// A key query with an identity (Ia), and a first device `GYKSNAWLVK`
    /// signed by Ia.
    pub fn key_query_with_identity_a() -> KeyQueryResponse {
        let data = response_from_file(&json!({
            "device_keys": {
                "@bob:localhost": {
                    "GYKSNAWLVK": Self::device_keys_payload_1_signed_by_a()
                }
            },
            "failures": {},
            "master_keys": Self::msk_a(),
            "self_signing_keys": Self::ssk_a(),
            "user_signing_keys": {}
        }));
        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    fn msk_b() -> Value {
        json!({
            "@bob:localhost": {
                "keys": {
                    "ed25519:NmI78hY54kE7OZsIjbRE/iCox59t4nzScCNEO6fvtY4": "NmI78hY54kE7OZsIjbRE/iCox59t4nzScCNEO6fvtY4"
                },
                "signatures": {
                    "@bob:localhost": {
                        "ed25519:ATWKQFSFRN": "MBOzCKYPQLQMpBY2lFZJ4c8451xJfQCdhPBb1AHlTUSxKFiWi6V+k1oRRnhQein/PjkIY7ZO+HoOrIeOtbRMAw",
                        "ed25519:NmI78hY54kE7OZsIjbRE/iCox59t4nzScCNEO6fvtY4": "xqLhC3sIUci1W2CNVW7HZWXreQApgjv2RDwB0WPiMd1P4vbZ/qJM0KWqK2piGPWliPi8YVREMrg216KXM3IhCA"
                    }
                },
                "usage": [
                    "master"
                ],
                "user_id": "@bob:localhost"
            }
        })
    }

    fn ssk_b() -> Value {
        json!({
            "@bob:localhost": {
                "keys": {
                    "ed25519:At1ai1VUZrCncCI7V7fEAJmBShfpqZ30xRzqcEjTjdc": "At1ai1VUZrCncCI7V7fEAJmBShfpqZ30xRzqcEjTjdc"
                },
                "signatures": {
                    "@bob:localhost": {
                        "ed25519:NmI78hY54kE7OZsIjbRE/iCox59t4nzScCNEO6fvtY4": "Ls6CeoA4LoPCHuSwG96kbhd1dEV09TgdMROIZi6vFz/MT9Wtik6joQi/tQ3zCwIZCSR53ksLO4jG1DD31AiBAA"
                    }
                },
                "usage": [
                    "self_signing"
                ],
                "user_id": "@bob:localhost"
            }
        })
    }

    fn device_keys_payload_2_signed_by_b() -> Value {
        json!({
            "algorithms": [
                "m.olm.v1.curve25519-aes-sha2",
                "m.megolm.v1.aes-sha2"
            ],
            "device_id": "ATWKQFSFRN",
            "keys": {
                "curve25519:ATWKQFSFRN": "CY0TWVK1/Kj3ZADuBcGe3UKvpT+IKAPMUsMeJhSDqno",
                "ed25519:ATWKQFSFRN": "TyTQqd6j2JlWZh97r+kTYuCbvqnPoNwO6EGovYsjY00"
            },
            "signatures": {
                "@bob:localhost": {
                    "ed25519:ATWKQFSFRN": "BQ9Gp0p+6srF+c8OyruqKKd9R4yaub3THYAyyBB/7X/rG8BwcAqFynzl1aGyFYun4Q+087a5OSiglCXI+/kQAA",
                    "ed25519:At1ai1VUZrCncCI7V7fEAJmBShfpqZ30xRzqcEjTjdc": "TWmDPaG7t0rZ6luauonELD3dmBDTIRryqXhgsIQRiGint2rJdic8RVyZ6a61bgu6mtBjfvU3prqMNp6sVi16Cg"
                }
            },
            "user_id": "@bob:localhost",
        })
    }
    /// A key query with a new identity (Ib) and a new device `ATWKQFSFRN`.
    /// `ATWKQFSFRN` is signed with the new identity but
    pub fn key_query_with_identity_b() -> KeyQueryResponse {
        let data = response_from_file(&json!({
            "device_keys": {
                "@bob:localhost": {
                    "ATWKQFSFRN": Self::device_keys_payload_2_signed_by_b(),
                    "GYKSNAWLVK": Self::device_keys_payload_1_signed_by_a(),
                }
            },
            "failures": {},
            "master_keys": Self::msk_b(),
            "self_signing_keys": Self::ssk_b(),
        }));
        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    /// A key query with a new identity (Ib) and a new device `ATWKQFSFRN`.
    /// `ATWKQFSFRN` is signed with the new identity but
    pub fn key_query_with_identity_no_identity() -> KeyQueryResponse {
        let data = response_from_file(&json!({
            "device_keys": {
                "@bob:localhost": {
                    "ATWKQFSFRN": Self::device_keys_payload_2_signed_by_b(),
                    "GYKSNAWLVK": Self::device_keys_payload_1_signed_by_a(),
                    "OPABMDDXGX": {
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                            "m.megolm.v1.aes-sha2"
                        ],
                        "device_id": "OPABMDDXGX",
                        "keys": {
                            "curve25519:OPABMDDXGX": "O6bwa9Op0E+PQPCrbTOfdYwU+j95RRPhXIHuNpe94ns",
                            "ed25519:OPABMDDXGX": "DvjkSNOM9XrR1gWrr2YSDvTnwnLIgKDMRr5v8HgMKak"
                        },
                        "signatures": {
                            "@bob:localhost": {
                                "ed25519:OPABMDDXGX": "o+BBnw/SIJWxSf799Adq6jEl9X3lwCg5MJkS8GlfId+pW3ReEETK0l+9bhCAgBsNSKRtB/fmZQBhjMx4FJr+BA"
                            }
                        },
                        "user_id": "@bob:localhost",
                        "unsigned": {
                            "device_display_name": "develop.element.io: Chrome on macOS"
                        }
                    }
                }
            },
            "failures": {},
        }));
        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }
}
