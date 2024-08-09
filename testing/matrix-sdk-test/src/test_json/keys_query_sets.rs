use ruma::{
    api::{client::keys::get_keys::v3::Response as KeyQueryResponse, IncomingResponse},
    device_id, user_id, DeviceId, UserId,
};
use serde_json::{json, Value};

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
        let data: Value = json!({
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

/// A set of keys query to test identity changes,
/// For user @bob, several payloads with no identities then identity A and B.
pub struct IdentityChangeDataSet {}

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

    pub fn msk_b() -> Value {
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

    pub fn ssk_b() -> Value {
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

    pub fn device_keys_payload_2_signed_by_b() -> Value {
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
    /// `ATWKQFSFRN` is signed with the new identity but `GYKSNAWLVK` is still
    /// signed by the old identity (Ia).
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

    /// A key query with no identity and a new device `OPABMDDXGX` (not
    /// cross-signed).
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
                    }
                }
            },
            "failures": {},
        }));
        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }
}

pub struct PreviouslyVerifiedTestData {}

#[allow(dead_code)]
impl PreviouslyVerifiedTestData {
    pub const MASTER_KEY_PRIVATE_EXPORT: &'static str =
        "bSa0nVTocZArMzL7OLmeFUIVF4ycp64rrkVMgqOYg6Y";
    pub const SELF_SIGNING_KEY_PRIVATE_EXPORT: &'static str =
        "MQ7b3MDXvOEMDvIOWkuH1XCNUyqBLqbdd1bT00p8HPU";
    pub const USER_SIGNING_KEY_PRIVATE_EXPORT: &'static str =
        "v77s+TlT5/NbcQym2B7Rwf20HOAhyInF2p1ZUYDPtow";

    pub fn own_id() -> &'static UserId {
        user_id!("@alice:localhost")
    }
    pub fn bob_id() -> &'static UserId {
        user_id!("@bob:localhost")
    }

    pub fn carol_id() -> &'static UserId {
        user_id!("@carol:localhost")
    }

    /// Current user keys query response containing the cross-signing keys
    pub fn own_keys_query_response_1() -> KeyQueryResponse {
        let data = json!({
            "master_keys": {
                "@alice:localhost": {
                    "keys": {
                        "ed25519:EPVg/QLG9+FmNvKjNXfycZEpQLtfHDaTN+rENAURZSk": "EPVg/QLG9+FmNvKjNXfycZEpQLtfHDaTN+rENAURZSk"
                    },
                    "signatures": {
                        "@alice:localhost": {
                            "ed25519:EPVg/QLG9+FmNvKjNXfycZEpQLtfHDaTN+rENAURZSk": "FX+srrw9SRmi12fexYHH1jrlEIWgOfre1aPNzDZWcAlaP9WKRdhcQGh70/3F9hk/PGr51I+ux62YgU4xnRTqAA",
                            "ed25519:PWVCNMMGCT": "teLq0rCYKX9h8WXu6kH8UE6HPKAtkF/DwCncxJGvVBCyZRtLHD8W1yYEzJXjTNynn+4fibQZBhR3th1RGLn4Ag"
                        }
                    },
                    "usage": [
                        "master"
                    ],
                    "user_id": "@alice:localhost"
                }
            },
            "self_signing_keys": {
                "@alice:localhost": {
                    "keys": {
                        "ed25519:WXLer0esHUanp8DCeu2Be0xB5ms9aKFFBrCFl50COjw": "WXLer0esHUanp8DCeu2Be0xB5ms9aKFFBrCFl50COjw"
                    },
                    "signatures": {
                        "@alice:localhost": {
                            "ed25519:EPVg/QLG9+FmNvKjNXfycZEpQLtfHDaTN+rENAURZSk": "lCV9R1xjD34arzq/CAuej1XBv+Ip4dFfAGHfe7znbW7rnwKDaX5PaX3MHk+EIC7nXvUYEAn502WcUFme5c0cCQ"
                        }
                    },
                    "usage": [
                        "self_signing"
                    ],
                    "user_id": "@alice:localhost"
                }
            },
            "user_signing_keys": {
                "@alice:localhost": {
                    "keys": {
                        "ed25519:MXob/N/bYI7U2655O1/AI9NOX1245RnE03Nl4Hvf+u0": "MXob/N/bYI7U2655O1/AI9NOX1245RnE03Nl4Hvf+u0"
                    },
                    "signatures": {
                        "@alice:localhost": {
                            "ed25519:EPVg/QLG9+FmNvKjNXfycZEpQLtfHDaTN+rENAURZSk": "A73QfZ5Dzhh7abdal/sEaq1bfgxzPFU8Bvwa9Y5TIe/a5jTmLVubNmsMSsO5tOT+b6aVJg1G4FtId0Q/cb1aAA"
                        }
                    },
                    "usage": [
                        "user_signing"
                    ],
                    "user_id": "@alice:localhost"
                }
            }
        });

        let data = response_from_file(&data);

        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    pub fn device_keys_payload_bob_unsigned_device() -> Value {
        json!({
            "algorithms": [
                "m.olm.v1.curve25519-aes-sha2",
                "m.megolm.v1.aes-sha2"
            ],
            "device_id": "XCYNVRMTER",
            "keys": {
                "curve25519:XCYNVRMTER": "xGKYkFcHGlJ+I1yiefPyZu1EY8i2h1eed5uk3PAW6GA",
                "ed25519:XCYNVRMTER": "EsU8MJzTYE+/VJs1K9HkGqb8UXCByPioynGrV28WocU"
            },
            "signatures": {
                "@bob:localhost": {
                    "ed25519:XCYNVRMTER": "yZ7cpaoA+0rRx+bmklsP1iAd0eGPH6gsdywC11VE98/mrcbeFuxjQVn39Ds7h+vmciu5GRzwWgDgv+6go6FHAQ",
                    // Remove the cross-signature
                    // "ed25519:e8JFSrW8LW3UK6SSXh2ZESUzptFbapr28/+WqndD+Xk": "xYnGmU9FEdoavB5P743gx3xbEy29tlfRX5lT3JO0dWhHdsP+muqBXUYMBl1RRFeZtIE0GYc9ORb6Yf88EdeoCw"
                }
            },
            "user_id": "@bob:localhost",
            "unsigned": {}
        })
    }

    pub fn bob_keys_query_response_signed() -> KeyQueryResponse {
        let data = json!({
            "device_keys": {
                "@bob:localhost": {
                    "RLZGZIHKMP": {
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                            "m.megolm.v1.aes-sha2"
                        ],
                        "device_id": "RLZGZIHKMP",
                        "keys": {
                            "curve25519:RLZGZIHKMP": "Zd8uO9Rr1PtqNno3//ybeUZ3JuqFtm17TQTWW0f47AU",
                            "ed25519:RLZGZIHKMP": "kH+Zn2m7LPES/XLOyVvnf8t4Byfj3mAbngHptHZFzk0"
                        },
                        "signatures": {
                            "@bob:localhost": {
                                "ed25519:RLZGZIHKMP": "w4MOkDiD+4XatQrRzGrcaqwVmiZrAjxmaIA8aSuzQveD2SJ2eVZq3OSpqx6QRUbG/gkkZxGmY13PkS/iAOv0AA",
                                "ed25519:e8JFSrW8LW3UK6SSXh2ZESUzptFbapr28/+WqndD+Xk": "ki+cV0EVe5cYXnzqU078qy1qu2rnaxaBQU+KwyvPpEUotNTXjWKUOJfxast42d5tjI5vsI5aiQ6XkYfjBJ74Bw"
                            }
                        },
                        "user_id": "@bob:localhost",
                        "unsigned": {}
                    },
                    "XCYNVRMTER": Self::device_keys_payload_bob_unsigned_device(),
                }
            },
            "failures": {},
            "master_keys": {
                "@bob:localhost": {
                    "keys": {
                        "ed25519:xZPyb4hxM8zaedDFz5m8HsDpX1fknd/V/69THLhNX9I": "xZPyb4hxM8zaedDFz5m8HsDpX1fknd/V/69THLhNX9I"
                    },
                    "signatures": {
                        "@bob:localhost": {
                            "ed25519:RLZGZIHKMP": "5bHLrx0HwYsNRtd65s1a1wVGlwgJU8yb8cq/Qbq04o9nVdQuY8+woQVWq9nxk59u6QFZIpFdVjXsuTPkDJLsBA",
                            "ed25519:xZPyb4hxM8zaedDFz5m8HsDpX1fknd/V/69THLhNX9I": "NA+cLNIPpmECcBIcmAH5l1K4IDXI6Xss1VmU8TZ04AYQSAh/2sv7NixEBO1/Raz0nErzkOl8gpRswHbHv1p7Dw"
                        },
                        "@alice:localhost": {
                            "ed25519:MXob/N/bYI7U2655O1/AI9NOX1245RnE03Nl4Hvf+u0": "n3X6afWYoSywqBpPlaDfQ2BNjl3ez5AzxEVwaB5/KEAzgwsq5B2qBW9N5uZaNWEq5M3JBrh0doj1FgUg4R3yBQ"
                        }
                    },
                    "usage": [
                        "master"
                    ],
                    "user_id": "@bob:localhost"
                }
            },
            "self_signing_keys": {
                "@bob:localhost": {
                    "keys": {
                        "ed25519:e8JFSrW8LW3UK6SSXh2ZESUzptFbapr28/+WqndD+Xk": "e8JFSrW8LW3UK6SSXh2ZESUzptFbapr28/+WqndD+Xk"
                    },
                    "signatures": {
                        "@bob:localhost": {
                            "ed25519:xZPyb4hxM8zaedDFz5m8HsDpX1fknd/V/69THLhNX9I": "kkGZHLY18jyqXs412VB31u6vxijbaBgVrIMR/LBAFULhTZk6HGH951N6NxMZnYHyH0sFaQhsl4DUqt7XthBHBQ"
                        }
                    },
                    "usage": [
                        "self_signing"
                    ],
                    "user_id": "@bob:localhost"
                }
            },
            "user_signing_keys": {}
        });

        let data = response_from_file(&data);

        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    pub fn bob_device_1_id() -> &'static DeviceId {
        device_id!("RLZGZIHKMP")
    }
    pub fn bob_device_2_id() -> &'static DeviceId {
        device_id!("XCYNVRMTER")
    }

    // Bob has a new identity, the two devices are properly self-signed
    pub fn bob_keys_query_response_rotated() -> KeyQueryResponse {
        let data = json!({
            "device_keys": {
                "@bob:localhost": {
                    "RLZGZIHKMP": {
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                            "m.megolm.v1.aes-sha2"
                        ],
                        "device_id": "RLZGZIHKMP",
                        "keys": {
                            "curve25519:RLZGZIHKMP": "Zd8uO9Rr1PtqNno3//ybeUZ3JuqFtm17TQTWW0f47AU",
                            "ed25519:RLZGZIHKMP": "kH+Zn2m7LPES/XLOyVvnf8t4Byfj3mAbngHptHZFzk0"
                        },
                        "signatures": {
                            "@bob:localhost": {
                                "ed25519:RLZGZIHKMP": "w4MOkDiD+4XatQrRzGrcaqwVmiZrAjxmaIA8aSuzQveD2SJ2eVZq3OSpqx6QRUbG/gkkZxGmY13PkS/iAOv0AA",
                                // "ed25519:At1ai1VUZrCncCI7V7fEAJmBShfpqZ30xRzqcEjTjdc": "rg3b3DovN3VztdcKyOcOlIGQxmm+8VC9+ImuXdgug/kPSi7QcljwOtjnk4LMkHexB3xVzB0ANcyNjbJ2cJuYBg",
                                "ed25519:e8JFSrW8LW3UK6SSXh2ZESUzptFbapr28/+WqndD+Xk": "ki+cV0EVe5cYXnzqU078qy1qu2rnaxaBQU+KwyvPpEUotNTXjWKUOJfxast42d5tjI5vsI5aiQ6XkYfjBJ74Bw"
                            }
                        },
                        "user_id": "@bob:localhost",
                        "unsigned": {
                            "device_display_name": "develop.element.io: Chrome on macOS"
                        }
                    },
                    "XCYNVRMTER": {
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                            "m.megolm.v1.aes-sha2"
                        ],
                        "device_id": "XCYNVRMTER",
                        "keys": {
                            "curve25519:XCYNVRMTER": "xGKYkFcHGlJ+I1yiefPyZu1EY8i2h1eed5uk3PAW6GA",
                            "ed25519:XCYNVRMTER": "EsU8MJzTYE+/VJs1K9HkGqb8UXCByPioynGrV28WocU"
                        },
                        "signatures": {
                            "@bob:localhost": {
                                "ed25519:XCYNVRMTER": "yZ7cpaoA+0rRx+bmklsP1iAd0eGPH6gsdywC11VE98/mrcbeFuxjQVn39Ds7h+vmciu5GRzwWgDgv+6go6FHAQ",
                                "ed25519:e8JFSrW8LW3UK6SSXh2ZESUzptFbapr28/+WqndD+Xk": "xYnGmU9FEdoavB5P743gx3xbEy29tlfRX5lT3JO0dWhHdsP+muqBXUYMBl1RRFeZtIE0GYc9ORb6Yf88EdeoCw",
                                "ed25519:NWoyMF4Ox8PEj+8l1e70zuIUg0D+wL9wtcj1KhWL0Bc": "2ieX8z+oW9JhdyIIkTDsQ2o5VWxcO6dOgeyPbRwbAL6Q8J6xujzYSIi568UAlPt+wg+RkNLshneexCPNMgSiDQ"
                            }
                        },
                        "user_id": "@bob:localhost",
                        "unsigned": {
                            "device_display_name": "app.element.io: Chrome on mac"
                        }
                    }
                }
            },
            "failures": {},
            "master_keys": {
                "@bob:localhost": {
                    "keys": {
                        "ed25519:xaFlsDqlDRRy7Idtt1dW9mdhH/gvvax34q+HxepjNWY": "xaFlsDqlDRRy7Idtt1dW9mdhH/gvvax34q+HxepjNWY"
                    },
                    "signatures": {
                        "@bob:localhost": {
                            "ed25519:XCYNVRMTER": "K1aPl+GtcNi8yDqn1zvKIJMg3PFLQkwoXJeFJMmct4SA2SiQIl1S2x1bDTC3kQ4/LA7ULiQgKlxkXdQVf2GZDw",
                            "ed25519:xaFlsDqlDRRy7Idtt1dW9mdhH/gvvax34q+HxepjNWY": "S5vw8moiPudKhmF1qIv3/ehbZ7uohJbcQaLcOV+DDh9iC/YX0UqnaGn1ZYWJpIN7Kxe2ZWCBwzp35DOVZKfxBw"
                        }
                    },
                    "usage": [
                        "master"
                    ],
                    "user_id": "@bob:localhost"
                }
            },
            "self_signing_keys": {
                "@bob:localhost": {
                    "keys": {
                        "ed25519:NWoyMF4Ox8PEj+8l1e70zuIUg0D+wL9wtcj1KhWL0Bc": "NWoyMF4Ox8PEj+8l1e70zuIUg0D+wL9wtcj1KhWL0Bc"
                    },
                    "signatures": {
                        "@bob:localhost": {
                            "ed25519:xaFlsDqlDRRy7Idtt1dW9mdhH/gvvax34q+HxepjNWY": "rwQIkR7JbZOrwGrmkW9QzFlK+lMjRDHVcGVlYNS/zVeDyvWxD0WFHcmy4p/LSgJDyrVt+th7LH7Bj+Ed/EGvCw"
                        }
                    },
                    "usage": [
                        "self_signing"
                    ],
                    "user_id": "@bob:localhost"
                }
            },
            "user_signing_keys": {}
        });

        let data = response_from_file(&data);

        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    pub fn device_1_keys_payload_carol() -> Value {
        json!({
            // Not self signed
            "algorithms": [
                "m.olm.v1.curve25519-aes-sha2",
                "m.megolm.v1.aes-sha2"
            ],
            "device_id": "BAZAPVEHGA",
            "keys": {
                "curve25519:BAZAPVEHGA": "/mCcWJb5mtNGPC7m4iQeW8gVJB4nG8z/z2QQXzzNijw",
                "ed25519:BAZAPVEHGA": "MLSoOlk27qcS/2O9Etp6XwgF8j+UT06yy/ypSeE9JRA"
            },
            "signatures": {
                "@carol:localhost": {
                    "ed25519:BAZAPVEHGA": "y2+Z0ofRRoNMj864SoAcNEXRToYVeiARu39CO0Vj2GcSIxlpR7B8K1wDYV4luP4gOL1t1tPgJPXL1WO//AHHCw",
                }
            },
            "user_id": "@carol:localhost"
        })
    }

    pub fn device_2_keys_payload_carol() -> Value {
        // Self-signed device
        json!({
            "algorithms": [
                "m.olm.v1.curve25519-aes-sha2",
                "m.megolm.v1.aes-sha2"
            ],
            "device_id": "JBRBCHOFDZ",
            "keys": {
                "curve25519:JBRBCHOFDZ": "900HdrlfxlH8yMSmEQ3C32uVyXCuxKs5oPKS/wUgzVQ",
                "ed25519:JBRBCHOFDZ": "BOINY06uroLYscHUq0e0FmUo/W0LC4/fsIPkZQe71NY"
            },
            "signatures": {
                "@carol:localhost": {
                    "ed25519:JBRBCHOFDZ": "MmSJS3yEdeuseiLTDCQwImZBPNFMdhhkAFjRZZrIONoGFR0AMSzgLtx/nSgXP8RwVxpycvb6OAqvSk2toK3PDg",
                    "ed25519:ZOMWgk5LAogkwDEdZl9Rv7FRGu0nGbeLtMHx6anzhQs": "VtoxmPn/BQVDlpEHPEI2wPUlruUX9m2zV3FChNkRyEEWur4St27WA1He8BwjVRiiT0bdUnVH3xfmucoV9UnbDA"
                }
            },
            "user_id": "@carol:localhost",
        })
    }

    pub fn ssk_payload_carol() -> Value {
        json!({
            "@carol:localhost": {
                "keys": {
                    "ed25519:ZOMWgk5LAogkwDEdZl9Rv7FRGu0nGbeLtMHx6anzhQs": "ZOMWgk5LAogkwDEdZl9Rv7FRGu0nGbeLtMHx6anzhQs"
                },
                "signatures": {
                    "@carol:localhost": {
                        "ed25519:itnwUCRfBPW08IrmBks9MTp/Qm5AJ2WNca13ptIZF8U": "thjR1/kxHADXqLqxc4Q3OZhAaLq7SPL96LNCGVGN64OYAJ5yG1cpqAXBiBCUaBUTdRTb0ys601RR8djPdTK/BQ"
                    }
                },
                "usage": [
                    "self_signing"
                ],
                "user_id": "@carol:localhost"
            }
        })
    }

    // Carol key query response with one signed and one unsigned device.
    // Bob has not verified Carol yet
    pub fn carol_keys_query_response_unsigned() -> KeyQueryResponse {
        let data = json!({
            "device_keys": {
                "@carol:localhost": {
                    "BAZAPVEHGA": Self::device_1_keys_payload_carol(),
                    "JBRBCHOFDZ": Self::device_2_keys_payload_carol()
                }
            },
            "failures": {},
            "master_keys": {
                "@carol:localhost": {
                    "keys": {
                        "ed25519:itnwUCRfBPW08IrmBks9MTp/Qm5AJ2WNca13ptIZF8U": "itnwUCRfBPW08IrmBks9MTp/Qm5AJ2WNca13ptIZF8U"
                    },
                    "signatures": {
                        "@carol:localhost": {
                            "ed25519:JBRBCHOFDZ": "eRA4jRSszQVuYpMtHTBuWGLEzcdUojyCW4/XKHRIQ2solv7iTC/MWES6I20YrHJa7H82CVoyNxS1Y3AwttBbCg",
                            "ed25519:itnwUCRfBPW08IrmBks9MTp/Qm5AJ2WNca13ptIZF8U": "e3r5L+JLv6FB8+Tt4BlIbz4wk2qPeMoKL1uR079qZzYMvtKoWGK9p000cZIhA5R1Tl7buQ9ODUfizued8g3TAg"
                        },
                        // "@alice:localhost": {
                        //     "ed25519:MXob/N/bYI7U2655O1/AI9NOX1245RnE03Nl4Hvf+u0": "yfRUvkaVg3KizC/HDXcuP4+gtYhxgzr8X916Wt4GRXjj4qhDjsCkf8mYZ7x4lcEXzRkYql5KelabgVzP12qmAA"
                        // }
                    },
                    "usage": [
                        "master"
                    ],
                    "user_id": "@carol:localhost"
                }
            },
            "self_signing_keys": Self::ssk_payload_carol(),
            "user_signing_keys": {}
        });

        let data = response_from_file(&data);

        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    pub fn carol_keys_query_response_signed() -> KeyQueryResponse {
        let data = json!({
            "device_keys": {
                "@carol:localhost": {
                    "BAZAPVEHGA": Self::device_1_keys_payload_carol(),
                    "JBRBCHOFDZ": Self::device_2_keys_payload_carol()
                }
            },
            "failures": {},
            "master_keys": {
                "@carol:localhost": {
                    "keys": {
                        "ed25519:itnwUCRfBPW08IrmBks9MTp/Qm5AJ2WNca13ptIZF8U": "itnwUCRfBPW08IrmBks9MTp/Qm5AJ2WNca13ptIZF8U"
                    },
                    "signatures": {
                        "@carol:localhost": {
                            "ed25519:JBRBCHOFDZ": "eRA4jRSszQVuYpMtHTBuWGLEzcdUojyCW4/XKHRIQ2solv7iTC/MWES6I20YrHJa7H82CVoyNxS1Y3AwttBbCg",
                            "ed25519:itnwUCRfBPW08IrmBks9MTp/Qm5AJ2WNca13ptIZF8U": "e3r5L+JLv6FB8+Tt4BlIbz4wk2qPeMoKL1uR079qZzYMvtKoWGK9p000cZIhA5R1Tl7buQ9ODUfizued8g3TAg"
                        },
                        "@alice:localhost": {
                            "ed25519:MXob/N/bYI7U2655O1/AI9NOX1245RnE03Nl4Hvf+u0": "yfRUvkaVg3KizC/HDXcuP4+gtYhxgzr8X916Wt4GRXjj4qhDjsCkf8mYZ7x4lcEXzRkYql5KelabgVzP12qmAA"
                        }
                    },
                    "usage": [
                        "master"
                    ],
                    "user_id": "@carol:localhost"
                }
            },
            "self_signing_keys": Self::ssk_payload_carol(),
            "user_signing_keys": {}
        });

        let data = response_from_file(&data);

        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }
}
