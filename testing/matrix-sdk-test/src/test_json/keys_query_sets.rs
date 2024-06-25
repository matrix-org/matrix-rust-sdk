use ruma::{
    api::{client::keys::get_keys::v3::Response as KeyQueryResponse, IncomingResponse},
    device_id, user_id, DeviceId, UserId,
};
use serde_json::json;

use crate::response_from_file;

/// This set of keys/query response was generated using a local synapse.
/// Each users was created, device added according to needs and the payload
/// of the keys query have been copy/pasted here.
///
/// The current user is `@me:localhost`, the private part of the
/// cross-signing keys have been exported using the console with the
/// following snippet:  `await mxMatrixClientPeg.get().getCrypto().
/// olmMachine.exportCrossSigningKeys()`.
///
/// They are imported in the test here in order to verify user signatures.
///
/// * `@me:localhost` is the current user mxId.
///
/// * `@dan:localhost` is a user with cross-signing enabled, with 2 devices. One
///   device (`JHPUERYQUW`) is self signed by @dan, but not the other one
///   (`FRGNMZVOKA`). `@me` has verified `@dan`, can be seen because `@dan`
///   master key has a signature by `@me` ssk
///
/// * `@dave` is a user that has not enabled cross-signing. And has one device
///   (`HVCXJTHMBM`).
///
///
/// * `@good` is a user with cross-signing enabled, with 2 devices. The 2
///   devices are properly signed by `@good` (i.e were self-verified by @good)
pub struct KeyDistributionTestData {}

#[allow(dead_code)]
impl KeyDistributionTestData {
    pub const MASTER_KEY_PRIVATE_EXPORT: &'static str =
        "9kquJqAtEUoTXljh5W2QSsCm4FH9WvWzIkDkIMUsM2k";
    pub const SELF_SIGNING_KEY_PRIVATE_EXPORT: &'static str =
        "QifnGfudByh/GpBgJYEMzq7/DGbp6fZjp58faQj3n1M";
    pub const USER_SIGNING_KEY_PRIVATE_EXPORT: &'static str =
        "zQSosK46giUFs2ACsaf32bA7drcIXbmViyEt+TLfloI";

    /// Current user keys query response containing the cross-signing keys
    pub fn me_keys_query_response() -> KeyQueryResponse {
        let data = json!({
            "master_keys": {
                "@me:localhost": {
                    "keys": {
                        "ed25519:KOS8zz9SJnMOxpfPOx9LO2+abuEcnZP/lxDo5RsXao4": "KOS8zz9SJnMOxpfPOx9LO2+abuEcnZP/lxDo5RsXao4"
                    },
                    "signatures": {
                        "@me:localhost": {
                            "ed25519:KOS8zz9SJnMOxpfPOx9LO2+abuEcnZP/lxDo5RsXao4": "5G9+Ns28rzNd+2DvP73Y0orr8sxduRQcrJj0YB7ZygH7oeXshvGLeQn6mcNs7q7ZrMR5bYlXxopufKSWWoKpCg",
                            "ed25519:YVKUSVBKWX": "ih1Kmj4dTB1AjjkwrLA2qIL3e/oPUFisP5Ic8kGp29wrpoHokasKKnkRl1zS7zq6iBcOL6aOZLPPX/ZHYCX5BQ"
                        }
                    },
                    "usage": [
                        "master"
                    ],
                    "user_id": "@me:localhost"
                }
            },
            "self_signing_keys": {
                "@me:localhost": {
                    "keys": {
                        "ed25519:9gXJQzvqZ+KQunfBTd0g9AkrulwEeFfspyWTSQFqqrw": "9gXJQzvqZ+KQunfBTd0g9AkrulwEeFfspyWTSQFqqrw"
                    },
                    "signatures": {
                        "@me:localhost": {
                            "ed25519:KOS8zz9SJnMOxpfPOx9LO2+abuEcnZP/lxDo5RsXao4": "amiKDLpWIwUQPzq+eov6KJsoskkWA1YzrGNb7HF3OcGV0nm4t7df0tUdZB/OpREtT5D78BKtzOPUipde2DxUAw"
                        }
                    },
                    "usage": [
                        "self_signing"
                    ],
                    "user_id": "@me:localhost"
                }
            },
            "user_signing_keys": {
                "@me:localhost": {
                    "keys": {
                        "ed25519:mvzOc2EuHoVfZTk1hX3y0hyjUs4MrfPv2V/PUFzMQJY": "mvzOc2EuHoVfZTk1hX3y0hyjUs4MrfPv2V/PUFzMQJY"
                    },
                    "signatures": {
                        "@me:localhost": {
                            "ed25519:KOS8zz9SJnMOxpfPOx9LO2+abuEcnZP/lxDo5RsXao4": "Cv56vTHAzRkvdcELleOlhECZQP0pXcikCdEZrnXbkjXQ/k0ZvVOJ1beG/SiH8xc6zh1bCIMYv96C9p8o+7VZCQ"
                        }
                    },
                    "usage": [
                        "user_signing"
                    ],
                    "user_id": "@me:localhost"
                }
            }
        });

        let data = response_from_file(&data);

        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    /// Dan has cross-signing setup, one device is cross signed `JHPUERYQUW`,
    /// but not the other one `FRGNMZVOKA`.
    /// `@dan` identity is signed by `@me` identity (alice trust dan)
    pub fn dan_keys_query_response() -> KeyQueryResponse {
        let data: serde_json::Value = json!({
                "device_keys": {
                    "@dan:localhost": {
                        "JHPUERYQUW": {
                            "algorithms": [
                                "m.olm.v1.curve25519-aes-sha2",
                                "m.megolm.v1.aes-sha2"
                            ],
                            "device_id": "JHPUERYQUW",
                            "keys": {
                                "curve25519:JHPUERYQUW": "PBo2nKbink/HxgzMrBftGPogsD0d47LlIMsViTpCRn4",
                                "ed25519:JHPUERYQUW": "jZ5Ca/J5RXn3qnNWIHFz9EQBZ4637QI/9ExSiEcGC7I"
                            },
                            "signatures": {
                                "@dan:localhost": {
                                    "ed25519:JHPUERYQUW": "PaVfCE9QODgluq0gYMpjCarfDbraRXU71uRcUN5MoqtiJYlB0bjzY6bD5/qxugrsgcx4DZOgCLgiyoEZ/vW4DQ",
                                    "ed25519:aX+O6rO/RxzkygPd7XXilKM07aSFK4gSPK1Zxenr6ak": "2sZcF5aSyEuryTfWgsw3rNDevnZisH2Df6fCO5pmGwweiaD+n6+pyrzB75mvA1sOwzm9jfTsjv/2+Uj1CNOTBA"
                                }
                            },
                            "user_id": "@dan:localhost",
                        },
                        "FRGNMZVOKA": {
                            "algorithms": [
                                "m.olm.v1.curve25519-aes-sha2",
                                "m.megolm.v1.aes-sha2"
                            ],
                            "device_id": "FRGNMZVOKA",
                            "keys": {
                                "curve25519:FRGNMZVOKA": "Hc/BC/xyQIEnScyZkEk+ilDMfOARxHMFoEcggPqqRw4",
                                "ed25519:FRGNMZVOKA": "jVroR0JoRemjF0vJslY3HirJgwfX5gm5DCM64hZgkI0"
                            },
                            "signatures": {
                                "@dan:localhost": {
                                    "ed25519:FRGNMZVOKA": "+row23EcWR2D8EKgwzZmy3dWz/l5DHvEHR6jHKnBohphEIsBl0o3Cp9rIztFpStFGRPSAa3xEqfMVW2dIaKkCg"
                                }
                            },
                            "user_id": "@dan:localhost",
                        },
                    }
                },
                "failures": {},
                "master_keys": {
                    "@dan:localhost": {
                        "keys": {
                            "ed25519:Nj4qZEmWplA8tofkjcR+YOvRCYMRLDKY71BT9GFO32k": "Nj4qZEmWplA8tofkjcR+YOvRCYMRLDKY71BT9GFO32k"
                        },
                        "signatures": {
                            "@dan:localhost": {
                                "ed25519:Nj4qZEmWplA8tofkjcR+YOvRCYMRLDKY71BT9GFO32k": "DI/zpWA/wG1tdK9aLof1TGBHtihtQZQ+7e62QRSBbo+RAHlQ+akGcaVskLbtLdEKbcJEt61F+Auol+XVGlCEBA",
                                "ed25519:SNEBMNPLHN": "5Y8byBteGZo1SvPf8QM88pvThJu+2mJ4020YsTLPhCQ4DfdalHWTPOvE7gw09cCONhX/cKY7YHMyH8R26Yd9DA"
                            },
                            "@me:localhost": {
                                "ed25519:mvzOc2EuHoVfZTk1hX3y0hyjUs4MrfPv2V/PUFzMQJY": "vg2MLJx36Usti4NfsbOfk0ipW7koOoTlBibZkQNrPTMX88V+geTgDjvIMEU/OAyEsgsDHjg3C+2t/yUUDE7hBA"
                            }
                        },
                        "usage": [
                            "master"
                        ],
                        "user_id": "@dan:localhost"
                    }
                },
                "self_signing_keys": {
                    "@dan:localhost": {
                        "keys": {
                            "ed25519:aX+O6rO/RxzkygPd7XXilKM07aSFK4gSPK1Zxenr6ak": "aX+O6rO/RxzkygPd7XXilKM07aSFK4gSPK1Zxenr6ak"
                        },
                        "signatures": {
                            "@dan:localhost": {
                                "ed25519:Nj4qZEmWplA8tofkjcR+YOvRCYMRLDKY71BT9GFO32k": "vxUCzOO4EGwLp+tzfoFbPOVicynvmWgxVx/bv/3fG/Xfl7piJVmeHP+1qDstOewiREuO4W+ti/tYkOXd7GgoAw"
                            }
                        },
                        "usage": [
                            "self_signing"
                        ],
                        "user_id": "@dan:localhost"
                    }
                },
                "user_signing_keys": {
                    "@dan:localhost": {
                        "keys": {
                            "ed25519:N4y+jN6GctRXyNDa1CFRdjofTTxHkNK9t430jE9DxrU": "N4y+jN6GctRXyNDa1CFRdjofTTxHkNK9t430jE9DxrU"
                        },
                        "signatures": {
                            "@dan:localhost": {
                                "ed25519:Nj4qZEmWplA8tofkjcR+YOvRCYMRLDKY71BT9GFO32k": "gbcD579EGVDRePnKV9j6YNwGhssgFeJWhF1NRJhFNAcpbGL8911cW54jyiFKFCev89QemfqyFFljldFLfyN9DA"
                            }
                        },
                        "usage": [
                            "user_signing"
                        ],
                        "user_id": "@dan:localhost"
                    }
                }
        });

        let data = response_from_file(&data);

        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    /// Same as `dan_keys_query_response` but `FRGNMZVOKA` was removed.
    pub fn dan_keys_query_response_device_loggedout() -> KeyQueryResponse {
        let data = json!({
                "device_keys": {
                    "@dan:localhost": {
                        "JHPUERYQUW": {
                            "algorithms": [
                                "m.olm.v1.curve25519-aes-sha2",
                                "m.megolm.v1.aes-sha2"
                            ],
                            "device_id": "JHPUERYQUW",
                            "keys": {
                                "curve25519:JHPUERYQUW": "PBo2nKbink/HxgzMrBftGPogsD0d47LlIMsViTpCRn4",
                                "ed25519:JHPUERYQUW": "jZ5Ca/J5RXn3qnNWIHFz9EQBZ4637QI/9ExSiEcGC7I"
                            },
                            "signatures": {
                                "@dan:localhost": {
                                    "ed25519:JHPUERYQUW": "PaVfCE9QODgluq0gYMpjCarfDbraRXU71uRcUN5MoqtiJYlB0bjzY6bD5/qxugrsgcx4DZOgCLgiyoEZ/vW4DQ",
                                    "ed25519:aX+O6rO/RxzkygPd7XXilKM07aSFK4gSPK1Zxenr6ak": "2sZcF5aSyEuryTfWgsw3rNDevnZisH2Df6fCO5pmGwweiaD+n6+pyrzB75mvA1sOwzm9jfTsjv/2+Uj1CNOTBA"
                                }
                            },
                            "user_id": "@dan:localhost",
                        },
                    }
                },
                "failures": {},
                "master_keys": {
                    "@dan:localhost": {
                        "keys": {
                            "ed25519:Nj4qZEmWplA8tofkjcR+YOvRCYMRLDKY71BT9GFO32k": "Nj4qZEmWplA8tofkjcR+YOvRCYMRLDKY71BT9GFO32k"
                        },
                        "signatures": {
                            "@dan:localhost": {
                                "ed25519:Nj4qZEmWplA8tofkjcR+YOvRCYMRLDKY71BT9GFO32k": "DI/zpWA/wG1tdK9aLof1TGBHtihtQZQ+7e62QRSBbo+RAHlQ+akGcaVskLbtLdEKbcJEt61F+Auol+XVGlCEBA",
                                "ed25519:SNEBMNPLHN": "5Y8byBteGZo1SvPf8QM88pvThJu+2mJ4020YsTLPhCQ4DfdalHWTPOvE7gw09cCONhX/cKY7YHMyH8R26Yd9DA"
                            },
                            "@me:localhost": {
                                "ed25519:mvzOc2EuHoVfZTk1hX3y0hyjUs4MrfPv2V/PUFzMQJY": "vg2MLJx36Usti4NfsbOfk0ipW7koOoTlBibZkQNrPTMX88V+geTgDjvIMEU/OAyEsgsDHjg3C+2t/yUUDE7hBA"
                            }
                        },
                        "usage": [
                            "master"
                        ],
                        "user_id": "@dan:localhost"
                    }
                },
                "self_signing_keys": {
                    "@dan:localhost": {
                        "keys": {
                            "ed25519:aX+O6rO/RxzkygPd7XXilKM07aSFK4gSPK1Zxenr6ak": "aX+O6rO/RxzkygPd7XXilKM07aSFK4gSPK1Zxenr6ak"
                        },
                        "signatures": {
                            "@dan:localhost": {
                                "ed25519:Nj4qZEmWplA8tofkjcR+YOvRCYMRLDKY71BT9GFO32k": "vxUCzOO4EGwLp+tzfoFbPOVicynvmWgxVx/bv/3fG/Xfl7piJVmeHP+1qDstOewiREuO4W+ti/tYkOXd7GgoAw"
                            }
                        },
                        "usage": [
                            "self_signing"
                        ],
                        "user_id": "@dan:localhost"
                    }
                },
                "user_signing_keys": {
                    "@dan:localhost": {
                        "keys": {
                            "ed25519:N4y+jN6GctRXyNDa1CFRdjofTTxHkNK9t430jE9DxrU": "N4y+jN6GctRXyNDa1CFRdjofTTxHkNK9t430jE9DxrU"
                        },
                        "signatures": {
                            "@dan:localhost": {
                                "ed25519:Nj4qZEmWplA8tofkjcR+YOvRCYMRLDKY71BT9GFO32k": "gbcD579EGVDRePnKV9j6YNwGhssgFeJWhF1NRJhFNAcpbGL8911cW54jyiFKFCev89QemfqyFFljldFLfyN9DA"
                            }
                        },
                        "usage": [
                            "user_signing"
                        ],
                        "user_id": "@dan:localhost"
                    }
                }
        });

        let data = response_from_file(&data);

        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    /// Dave is a user that has not enabled cross-signing
    pub fn dave_keys_query_response() -> KeyQueryResponse {
        let data = json!({
            "device_keys": {
                "@dave:localhost": {
                    "HVCXJTHMBM": {
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                            "m.megolm.v1.aes-sha2"
                        ],
                        "device_id": "HVCXJTHMBM",
                        "keys": {
                            "curve25519:HVCXJTHMBM": "0GPOoQwhAGVu1lIvOZway3/XjdxVNHEi5z/4by8TzxU",
                            "ed25519:HVCXJTHMBM": "/4ZzD1Ou70/Ojj5aaPqBopCN8SzQpKM7itiWZ/07fXc"
                        },
                        "signatures": {
                            "@dave:localhost": {
                                "ed25519:HVCXJTHMBM": "b1DV7xN2My2oXbZVVtTeJR9hzXIg1Cx4h+W51+tVq5GAoSYtrWR31PyKPROk28CvQ9Pu++/jdomaW7/oYPxoCg",
                            }
                        },
                        "user_id": "@dave:localhost",
                    }
                }
            }
        });

        let data = response_from_file(&data);

        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    /// Good is a user that has all his devices correctly cross-signed
    pub fn good_keys_query_response() -> KeyQueryResponse {
        let data = json!({
            "device_keys": {
                "@good:localhost": {
                    "JAXGBVZYLA": {
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                            "m.megolm.v1.aes-sha2"
                        ],
                        "device_id": "JAXGBVZYLA",
                        "keys": {
                            "curve25519:JAXGBVZYLA": "a4vWxnHUKvELfB7WYLCW07vEbwybZReyKReWHxQhgW0",
                            "ed25519:JAXGBVZYLA": "m22nVxqJK72iph+FhOMqX/MDd7AoF9BJ033MlMLnDCg"
                        },
                        "signatures": {
                            "@good:localhost": {
                                "ed25519:JAXGBVZYLA": "EXKQiXNKjWSE76WxF8TUvxjCyw/qsV27gcbsgpSN1zzHzGzVdY1Qr4EB8t/76SL5rZP/9hqcAvqPSJW/N7iKCg",
                                "ed25519:YwQVBWn2sA5lLqp/dQsNk7fiYOuQuQhujefOPjejc+U": "sXJUXKE7hqXnsNbqlzS/1MGlGmeJU54v6/UMWAs+6bCzOFUC1+uqU1KlzfmpsVG3MKxR4r/ZLZdxoKVfUuQMAA"
                            }
                        },
                        "user_id": "@good:localhost"
                    },
                    "ZGLCFWEPCY": {
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                            "m.megolm.v1.aes-sha2"
                        ],
                        "device_id": "ZGLCFWEPCY",
                        "keys": {
                            "curve25519:ZGLCFWEPCY": "kfcIEf6ZRgTP184yuIJYabfsBFsGXiVQE/cyW9qYnQA",
                            "ed25519:ZGLCFWEPCY": "WLSA1tSe0eOZCeESH5WMb9cp3AgRZzm4ooSud+NwcEw"
                        },
                        "signatures": {
                            "@good:localhost": {
                                "ed25519:ZGLCFWEPCY": "AVXFgHk/QcAbOVBF5Xu4OW+03CZKBs2qAYh0fjIA49r+X+aX7QIKrbRyXU/ictPBLMpj1yXF+2J5vwR/KQYVCA",
                                "ed25519:YwQVBWn2sA5lLqp/dQsNk7fiYOuQuQhujefOPjejc+U": "VZk70FWiYN/YSwGykt2CygcOl1bq2D+dVSSKBL5GA5uHXxt6ypDlYvtWprM1l7re3llp5j105MevsjQ+2sWmCw"
                            }
                        },
                        "user_id": "@good:localhost"
                    }
                }
            },
            "failures": {},
            "master_keys": {
                "@good:localhost": {
                    "keys": {
                        "ed25519:5vTK2S2wVXo4xGT4BhcwpINVjRLjorkkJgCjnrHgtl8": "5vTK2S2wVXo4xGT4BhcwpINVjRLjorkkJgCjnrHgtl8"
                    },
                    "signatures": {
                        "@good:localhost": {
                            "ed25519:5vTK2S2wVXo4xGT4BhcwpINVjRLjorkkJgCjnrHgtl8": "imAhrTIlPuf6hNqlbcSUnC2ndZPk5NwQLzbi9kZ8nmnPGjmv39f4U4Vh/KiweqQnI4ActGpcYyM7k9S2Ef8/CQ",
                            "ed25519:HPNYOQGUEE": "6w3egsvd+oVPCclef+hF1CfFMZrGTf/plFvPU5iP69WNw4w0UPAKSV1jOzh7Wv4LVGX5O3afjA9DG+O7aHZmBw"
                        }
                    },
                    "usage": [
                        "master"
                    ],
                    "user_id": "@good:localhost"
                }
            },
            "self_signing_keys": {
                "@good:localhost": {
                    "keys": {
                        "ed25519:YwQVBWn2sA5lLqp/dQsNk7fiYOuQuQhujefOPjejc+U": "YwQVBWn2sA5lLqp/dQsNk7fiYOuQuQhujefOPjejc+U"
                    },
                    "signatures": {
                        "@good:localhost": {
                            "ed25519:5vTK2S2wVXo4xGT4BhcwpINVjRLjorkkJgCjnrHgtl8": "2AyR8lovFv8J1DwPwdCsAM9Tw877QhaVHmVkPopsmSokS2fst8LDQtsg/PiftVc+74NGz5tnYIMDxn4BjAisAg"
                        }
                    },
                    "usage": [
                        "self_signing"
                    ],
                    "user_id": "@good:localhost"
                }
            },
            "user_signing_keys": {
                "@good:localhost": {
                    "keys": {
                        "ed25519:u1PwO3/a/HTnN9IF7BVa2dJQ7bc00J22eNS0vM4FjTA": "u1PwO3/a/HTnN9IF7BVa2dJQ7bc00J22eNS0vM4FjTA"
                    },
                    "signatures": {
                        "@good:localhost": {
                            "ed25519:5vTK2S2wVXo4xGT4BhcwpINVjRLjorkkJgCjnrHgtl8": "88v9/Z3TJeY2lsu3cFQaEuhHH5ixjJs22ALQRKY+O6VPGCT/BAzH6kUb7teinFfpvQjoXN3t5fVJxbP9mVlxDg"
                        }
                    },
                    "usage": [
                        "user_signing"
                    ],
                    "user_id": "@good:localhost"
                }
            }
        });

        let data = response_from_file(&data);

        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    pub fn me_id() -> &'static UserId {
        user_id!("@me:localhost")
    }

    pub fn me_device_id() -> &'static DeviceId {
        device_id!("ABCDEFGH")
    }

    pub fn dan_unsigned_device_id() -> &'static DeviceId {
        device_id!("FRGNMZVOKA")
    }

    pub fn dan_signed_device_id() -> &'static DeviceId {
        device_id!("JHPUERYQUW")
    }

    pub fn dave_device_id() -> &'static DeviceId {
        device_id!("HVCXJTHMBM")
    }

    pub fn dan_id() -> &'static UserId {
        user_id!("@dan:localhost")
    }

    pub fn dave_id() -> &'static UserId {
        user_id!("@dave:localhost")
    }

    pub fn good_id() -> &'static UserId {
        user_id!("@good:localhost")
    }
}
