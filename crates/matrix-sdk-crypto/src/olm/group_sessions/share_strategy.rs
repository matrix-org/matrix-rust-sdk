// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Deref,
};

use itertools::{Either, Itertools};
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, UserId};
use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

use super::OutboundGroupSession;
use crate::{
    error::OlmResult, store::Store, types::events::room_key_withheld::WithheldCode,
    EncryptionSettings, ReadOnlyDevice,
};

/// Returned by `collect_session_recipients`.
///
/// Information indicating whether the session needs to be rotated
/// (`should_rotate`) and the list of users/devices that should receive
/// (`devices`) or not the session,  including withheld reason
/// `withheld_devices`.
#[derive(Debug)]
pub(crate) struct CollectRecipientsResult {
    /// If true the outbound group session should be rotated
    pub should_rotate: bool,
    /// The map of user|device that should receive the session
    pub devices: BTreeMap<OwnedUserId, Vec<ReadOnlyDevice>>,
    /// The map of user|device that won't receive the key with the withheld
    /// code.
    pub withheld_devices: Vec<(ReadOnlyDevice, WithheldCode)>,
}

struct KeySharingResult {
    /// The map of user|device that should have access to current room key
    pub allowed_devices: BTreeMap<OwnedUserId, Vec<ReadOnlyDevice>>,
    /// The map of user|device that won't receive the room key with the withheld
    /// code.
    pub withheld_devices: Vec<(ReadOnlyDevice, WithheldCode)>,
}

trait RoomKeyShareStrategy {
    async fn collect_session_recipients(
        &self,
        store: &Store,
        users: BTreeSet<&UserId>,
    ) -> OlmResult<KeySharingResult>;
}

/// Defines the sharing strategy for the room key. That is what devices are
/// included in the conversation, and what are not.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RoomKeySharingStrategy {
    /** Simple strategy, send to all devices or only to trusted devices */
    Legacy(DeviceBasedStrategy),
    /** Cross-Signing TOFU strategy. Will only distribute keys to devices
     * that are cross-signed by their owner */
    Modern(IdentityBasedStrategy),
}

impl Default for RoomKeySharingStrategy {
    fn default() -> Self {
        RoomKeySharingStrategy::Legacy(DeviceBasedStrategy { only_allow_trusted_devices: false })
    }
}

/// Device based sharing strategy.
/// With this strategy every device will be considered for sharing, looking at
/// their individual trust level.
/// By default all devices will receive the key unless
/// `only_allow_trusted_devices` is set to true. Individual devices can be
/// blacklisted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceBasedStrategy {
    /// If `true`, devices that are not trusted will be excluded from the
    /// conversation. A device is trusted if any of the following is true:
    ///     - It was manually marked as trusted.
    ///     - It was marked as verified via interactive verification.
    ///     - It is signed by it's owner identity, and this identity has been
    ///       trusted via interactive verification.
    ///     - It is the current own device of the user.
    pub only_allow_trusted_devices: bool,
}

/// Idenity based sharing strategy.
/// In this mode only user identities are considered, and all devices that are
/// signed by their owner identity will receive the keys.
/// Users with no identity (cross-signing not enabled) will not receive any key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IdentityBasedStrategy {
    // pub throw_on_unknown_identy: bool,
}

impl RoomKeySharingStrategy {
    /// Creates a new legacy strategy, based on per device trust.
    pub const fn new_legacy(only_allow_trusted_devices: bool) -> Self {
        return Self::Legacy(DeviceBasedStrategy { only_allow_trusted_devices });
    }

    /// Creates a new modern strategy, based on per identity trust.
    pub const fn new_modern() -> Self {
        return Self::Modern(IdentityBasedStrategy {});
    }

    pub(crate) async fn collect_session_recipients(
        &self,
        settings: &EncryptionSettings,
        store: &Store,
        users: impl Iterator<Item = &UserId>,
        outbound: &OutboundGroupSession,
    ) -> OlmResult<CollectRecipientsResult> {
        let users: BTreeSet<&UserId> = users.collect();

        let users_shared_with: BTreeSet<OwnedUserId> =
            outbound.shared_with_set.read().unwrap().keys().cloned().collect();

        let users_shared_with: BTreeSet<&UserId> =
            users_shared_with.iter().map(Deref::deref).collect();

        // A user left if a user is missing from the set of users that should
        // get the session but is in the set of users that received the session.
        let user_left = !users_shared_with.difference(&users).collect::<BTreeSet<_>>().is_empty();

        let visibility_changed =
            outbound.settings().history_visibility != settings.history_visibility;
        let algorithm_changed = outbound.settings().algorithm != settings.algorithm;

        let sharing_strategy_changed =
            outbound.settings().sharing_strategy != settings.sharing_strategy;

        let mut should_rotate =
            user_left || visibility_changed || algorithm_changed || sharing_strategy_changed;

        if should_rotate {
            debug!(
                should_rotate,
                user_left,
                visibility_changed,
                algorithm_changed,
                "Rotating room key to protect room history",
            );
        }

        let sharing_result = match self {
            RoomKeySharingStrategy::Legacy(legacy) => {
                legacy.collect_session_recipients(store, users).await?
            }
            RoomKeySharingStrategy::Modern(modern) => {
                modern.collect_session_recipients(store, users).await?
            }
        };

        // If we haven't already concluded that the session should be
        // rotated for other reasons, we also need to check whether any
        // of the devices in the session got deleted or blacklisted in the
        // meantime. If so, we should also rotate the session.
        if !should_rotate {
            for (user_id, devices) in &sharing_result.allowed_devices {
                // Device IDs that should receive this session
                let recipient_device_ids: BTreeSet<&DeviceId> =
                    devices.iter().map(|d| d.device_id()).collect();

                if let Some(shared) = outbound.shared_with_set.read().unwrap().get(user_id) {
                    // Devices that received this session
                    let shared: BTreeSet<OwnedDeviceId> = shared.keys().cloned().collect();
                    let shared: BTreeSet<&DeviceId> = shared.iter().map(|d| d.as_ref()).collect();

                    // The set difference between
                    //
                    // 1. Devices that had previously received the session, and
                    // 2. Devices that would now receive the session
                    //
                    // Represents newly deleted, unauthorised or blacklisted devices. If this
                    // set is non-empty, we must rotate.
                    let newly_excluded =
                        shared.difference(&recipient_device_ids).collect::<BTreeSet<_>>();

                    should_rotate = !newly_excluded.is_empty();
                    if should_rotate {
                        // We have concluded that it should rotate, no need to continue checking
                        // other users
                        debug!(
                                "Rotating a room key due to at least these devices being deleted/blacklisted {:?}",
                                newly_excluded,
                            );
                        break;
                    }
                };
            }
        }

        trace!(should_rotate, "Done calculating group session recipients");

        Ok(CollectRecipientsResult {
            should_rotate,
            devices: sharing_result.allowed_devices,
            withheld_devices: sharing_result.withheld_devices,
        })
    }
}

impl RoomKeyShareStrategy for DeviceBasedStrategy {
    async fn collect_session_recipients(
        &self,
        store: &Store,
        users: BTreeSet<&UserId>,
    ) -> OlmResult<KeySharingResult> {
        let mut allowed_devices: BTreeMap<OwnedUserId, Vec<ReadOnlyDevice>> = Default::default();
        let mut withheld_devices: Vec<(ReadOnlyDevice, WithheldCode)> = Default::default();

        trace!(?users, ?self, "Calculating group session recipients");

        let own_identity =
            store.get_user_identity(store.user_id()).await?.and_then(|i| i.into_own());

        for user_id in users {
            let user_devices = store.get_readonly_devices_filtered(user_id).await?;

            // We only need the user identity if settings.only_allow_trusted_devices is set.
            let device_owner_identity = if self.only_allow_trusted_devices {
                store.get_user_identity(user_id).await?
            } else {
                None
            };

            // From all the devices a user has, we're splitting them into two
            // buckets, a bucket of devices that should receive the
            // room key and a bucket of devices that should receive
            // a withheld code.
            let (recipients, withheld_recipients): (
                Vec<ReadOnlyDevice>,
                Vec<(ReadOnlyDevice, WithheldCode)>,
            ) = user_devices.into_values().partition_map(|d| {
                if d.is_blacklisted() {
                    Either::Right((d, WithheldCode::Blacklisted))
                } else if self.only_allow_trusted_devices
                    && !d.is_verified(&own_identity, &device_owner_identity)
                {
                    Either::Right((d, WithheldCode::Unverified))
                } else {
                    Either::Left(d)
                }
            });

            if recipients.len() > 0 {
                allowed_devices.entry(user_id.to_owned()).or_default().extend(recipients);
            }
            withheld_devices.extend(withheld_recipients);
        }

        Ok(KeySharingResult { allowed_devices, withheld_devices })
    }
}

impl RoomKeyShareStrategy for IdentityBasedStrategy {
    async fn collect_session_recipients(
        &self,
        store: &Store,
        users: BTreeSet<&UserId>,
    ) -> OlmResult<KeySharingResult> {
        let mut allowed_devices: BTreeMap<OwnedUserId, Vec<ReadOnlyDevice>> = Default::default();
        let mut withheld_devices: Vec<(ReadOnlyDevice, WithheldCode)> = Default::default();

        trace!(?users, ?self, "Calculating group session recipients");

        for user_id in users {
            let user_devices = store.get_readonly_devices_filtered(user_id).await?;

            // Get device owner identity
            let device_owner_identity = store.get_user_identity(user_id).await?;

            match device_owner_identity {
                None => {
                    // withheld all the users devices, we need to have an identity for TOFU
                    // distribution
                    withheld_devices.extend(
                        user_devices.into_values().map(|d| (d, WithheldCode::Unauthorised)),
                    );
                }
                Some(device_owner_identity) => {
                    // From all the devices a user has, we're splitting them into two
                    // buckets, a bucket of devices that should receive the
                    // room key and a bucket of devices that should receive
                    // a withheld code.
                    let (recipients, withheld_recipients): (
                        Vec<ReadOnlyDevice>,
                        Vec<(ReadOnlyDevice, WithheldCode)>,
                    ) = user_devices.into_values().partition_map(|d| {
                        if d.is_cross_signing_by_owner(&device_owner_identity) {
                            Either::Left(d)
                        } else {
                            Either::Right((d, WithheldCode::Unauthorised))
                        }
                    });

                    if recipients.len() > 0 {
                        allowed_devices.entry(user_id.to_owned()).or_default().extend(recipients);
                    }
                    withheld_devices.extend(withheld_recipients);
                }
            }
        }

        Ok(KeySharingResult { allowed_devices, withheld_devices })
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use matrix_sdk_test::{async_test, response_from_file};
    use ruma::{
        api::{client::keys::get_keys, IncomingResponse},
        device_id, room_id, user_id, DeviceId, TransactionId, UserId,
    };
    use serde_json::json;

    use super::RoomKeySharingStrategy;
    use crate::{
        olm::OutboundGroupSession, types::events::room_key_withheld::WithheldCode,
        CrossSigningKeyExport, EncryptionSettings, OlmMachine,
    };

    /// ============================================================================
    /// ============================================================================
    ///
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
    /// * `@dan:localhost` is a user with cross-signing enabled, with 2 devices.
    ///   One device (`JHPUERYQUW`) is self signed by @dan, but not the other
    ///   one (`FRGNMZVOKA`). `@me` has verified `@dan`, can be seen because
    ///   `@dan` master key has a signature by `@me` ssk
    ///
    /// * `@dave` is a user that has not enabled cross-signing. And has one
    ///   device (`HVCXJTHMBM`).
    ///
    ///
    /// * `@good` is a user with cross-signing enabled, with 2 devices. The 2
    ///   devices are properly signed by `@good` (i.e were self-verified by
    ///   @good)
    /// ============================================================================
    /// ============================================================================

    /// Current user keys query response containing the cross-signing keys
    fn me_keys_query_response() -> get_keys::v3::Response {
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

        get_keys::v3::Response::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    /// Dan has cross-signing setup, one device is cross signed `JHPUERYQUW`,
    /// but not the other one `FRGNMZVOKA`.
    /// `@dan` identity is signed by `@me` identity (alice trust dan)
    fn dan_keys_query_response() -> get_keys::v3::Response {
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

        get_keys::v3::Response::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    /// Dave is a user that has not enabled cross-signing
    fn dave_keys_query_response() -> get_keys::v3::Response {
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

        get_keys::v3::Response::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    /// Good is a user that has all his devices correctly cross-signed
    fn good_keys_query_response() -> get_keys::v3::Response {
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

        get_keys::v3::Response::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    fn me_id() -> &'static UserId {
        user_id!("@me:localhost")
    }

    fn me_device_id() -> &'static DeviceId {
        device_id!("ABCDEFGH")
    }

    fn dan_id() -> &'static UserId {
        user_id!("@dan:localhost")
    }

    fn dave_id() -> &'static UserId {
        user_id!("@dave:localhost")
    }

    fn good_id() -> &'static UserId {
        user_id!("@good:localhost")
    }

    async fn set_up_test_machine() -> OlmMachine {
        let machine = OlmMachine::new(me_id(), me_device_id()).await;

        let keys_query = me_keys_query_response();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        machine
            .import_cross_signing_keys(CrossSigningKeyExport {
                master_key: Some("9kquJqAtEUoTXljh5W2QSsCm4FH9WvWzIkDkIMUsM2k".to_owned()),
                self_signing_key: Some("QifnGfudByh/GpBgJYEMzq7/DGbp6fZjp58faQj3n1M".to_owned()),
                user_signing_key: Some("zQSosK46giUFs2ACsaf32bA7drcIXbmViyEt+TLfloI".to_owned()),
            })
            .await
            .unwrap();

        let keys_query = dan_keys_query_response();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let txn_id_dave = TransactionId::new();
        let keys_query_dave = dave_keys_query_response();
        machine.mark_request_as_sent(&txn_id_dave, &keys_query_dave).await.unwrap();

        let txn_id_good = TransactionId::new();
        let keys_query_good = good_keys_query_response();
        machine.mark_request_as_sent(&txn_id_good, &keys_query_good).await.unwrap();

        machine
    }

    /// Perform a key share for the same set of users but with different
    /// strategy.
    #[async_test]
    async fn test_share_with_per_device_strategy_to_all() {
        let machine = set_up_test_machine().await;

        let legacy_strategy = RoomKeySharingStrategy::new_legacy(false);

        let encryption_settings =
            EncryptionSettings { sharing_strategy: legacy_strategy, ..Default::default() };

        let fake_room_id = room_id!("!roomid:localhost");

        let id_keys = machine.identity_keys();
        let group_session = OutboundGroupSession::new(
            machine.device_id().into(),
            Arc::new(id_keys),
            fake_room_id,
            encryption_settings.clone(),
        )
        .unwrap();

        let share_result = encryption_settings
            .sharing_strategy
            .collect_session_recipients(
                &encryption_settings,
                machine.store(),
                vec![dan_id(), dave_id(), good_id()].into_iter(),
                &group_session,
            )
            .await
            .unwrap();

        assert!(!share_result.should_rotate);

        let dan_devices_shared = share_result.devices.get(dan_id()).unwrap();
        let dave_devices_shared = share_result.devices.get(dave_id()).unwrap();
        let good_devices_shared = share_result.devices.get(good_id()).unwrap();

        // With this strategy the room key would be distributed to all devices
        assert_eq!(dan_devices_shared.len(), 2);
        assert_eq!(dave_devices_shared.len(), 1);
        assert_eq!(good_devices_shared.len(), 2);
    }

    #[async_test]
    async fn test_share_with_per_device_strategy_only_trusted() {
        let machine = set_up_test_machine().await;

        let fake_room_id = room_id!("!roomid:localhost");

        let legacy_strategy = RoomKeySharingStrategy::new_legacy(true);

        let encryption_settings =
            EncryptionSettings { sharing_strategy: legacy_strategy, ..Default::default() };

        let id_keys = machine.identity_keys();
        let group_session = OutboundGroupSession::new(
            machine.device_id().into(),
            Arc::new(id_keys),
            fake_room_id,
            encryption_settings.clone(),
        )
        .unwrap();

        let share_result = encryption_settings
            .sharing_strategy
            .collect_session_recipients(
                &encryption_settings,
                machine.store(),
                vec![dan_id(), dave_id(), good_id()].into_iter(),
                &group_session,
            )
            .await
            .unwrap();

        assert!(!share_result.should_rotate);

        let dave_devices_shared = share_result.devices.get(dave_id());
        let good_devices_shared = share_result.devices.get(good_id());
        tracing::debug!(?dave_devices_shared, "Dave share result is");
        // dave and good wouldn't receive any key
        assert!(dave_devices_shared.is_none());
        assert!(good_devices_shared.is_none());

        // dan is verified ny me and has one of his devices self signed, so should get
        // the key
        let dan_devices_shared = share_result.devices.get(dan_id()).unwrap();

        assert_eq!(dan_devices_shared.len(), 1);
        let dan_device_that_will_get_the_key = &dan_devices_shared[0];
        assert_eq!(dan_device_that_will_get_the_key.device_id().as_str(), "JHPUERYQUW");
    }

    /// Perform a key share for the same set of users but with different
    /// strategy.
    #[async_test]
    async fn test_share_with_per_identity_strategy() {
        let machine = set_up_test_machine().await;

        let modern_strategy = RoomKeySharingStrategy::new_modern();

        let encryption_settings =
            EncryptionSettings { sharing_strategy: modern_strategy, ..Default::default() };

        let fake_room_id = room_id!("!roomid:localhost");

        let id_keys = machine.identity_keys();
        let group_session = OutboundGroupSession::new(
            machine.device_id().into(),
            Arc::new(id_keys),
            fake_room_id,
            encryption_settings.clone(),
        )
        .unwrap();

        let share_result = encryption_settings
            .sharing_strategy
            .collect_session_recipients(
                &encryption_settings,
                machine.store(),
                vec![dan_id(), dave_id(), good_id()].into_iter(),
                &group_session,
            )
            .await
            .unwrap();

        assert!(!share_result.should_rotate);

        let dan_devices_shared = share_result.devices.get(dan_id()).unwrap();

        // With this strategy the key would be only distributed to 1 of dan devices, the
        // only one that is signed by dan.
        assert_eq!(dan_devices_shared.len(), 1);
        let dan_device_that_will_get_the_key = &dan_devices_shared[0];
        assert_eq!(dan_device_that_will_get_the_key.device_id().as_str(), "JHPUERYQUW");
        // The other device should reveive a withheld code
        let (_, code) = share_result
            .withheld_devices
            .iter()
            .filter(|(d, _)| d.device_id().as_str() == "FRGNMZVOKA")
            .nth(0)
            .expect("This dan's device should receive a withheld codee");
        assert_eq!(code.as_str(), WithheldCode::Unauthorised.as_str());

        // With this strategy the key would not be distributed to any dave devices as he
        // has not setup cross-signing
        let dave_devices_shared = share_result.devices.get(dave_id());
        assert!(dave_devices_shared.is_none());
        // The other device should reveive a withheld code
        let (_, code) = share_result
            .withheld_devices
            .iter()
            .filter(|(d, _)| d.device_id().as_str() == "HVCXJTHMBM")
            .nth(0)
            .expect("This dan's device should receive a withheld codee");
        assert_eq!(code.as_str(), WithheldCode::Unauthorised.as_str());

        // Good has all his devices signed correctly, so they should all get the key
        let good_devices_shared = share_result.devices.get(good_id()).unwrap();
        assert_eq!(good_devices_shared.len(), 2);
    }

    /// In that test we try to share to `@good:localhost.com`. For this user the
    /// 2 strategies will share to the same set of devices, nonetheless test
    /// that changing the strategy forces a session rotation.
    #[async_test]
    async fn test_rotate_key_when_sharing_strategy_changes() {
        let machine = set_up_test_machine().await;

        let legacy_strategy = RoomKeySharingStrategy::new_legacy(false);

        let encryption_settings =
            EncryptionSettings { sharing_strategy: legacy_strategy, ..Default::default() };

        let fake_room_id = room_id!("!roomid:localhost");

        let id_keys = machine.identity_keys();
        let group_session = OutboundGroupSession::new(
            machine.device_id().into(),
            Arc::new(id_keys),
            fake_room_id,
            encryption_settings.clone(),
        )
        .unwrap();

        let _ = encryption_settings
            .sharing_strategy
            .collect_session_recipients(
                &encryption_settings,
                machine.store(),
                vec![good_id()].into_iter(),
                &group_session,
            )
            .await
            .unwrap();

        // now share with another strategy
        let identity_strategy = RoomKeySharingStrategy::new_modern();
        let encryption_settings =
            EncryptionSettings { sharing_strategy: identity_strategy, ..Default::default() };

        let result = encryption_settings
            .sharing_strategy
            .collect_session_recipients(
                &encryption_settings,
                machine.store(),
                vec![good_id()].into_iter(),
                &group_session,
            )
            .await
            .unwrap();

        // In that case no devices have left follwing the strategy change, yet as we
        // have changed the strategy it should rotate
        assert!(result.should_rotate)
    }
}
