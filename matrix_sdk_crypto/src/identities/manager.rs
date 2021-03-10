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
    collections::{BTreeMap, HashSet},
    convert::TryFrom,
    sync::Arc,
};
use tracing::{info, trace, warn};

use matrix_sdk_common::{
    api::r0::keys::get_keys::Response as KeysQueryResponse,
    encryption::DeviceKeys,
    identifiers::{DeviceId, DeviceIdBox, UserId},
};

use crate::{
    error::OlmResult,
    identities::{
        MasterPubkey, OwnUserIdentity, ReadOnlyDevice, SelfSigningPubkey, UserIdentities,
        UserIdentity, UserSigningPubkey,
    },
    requests::KeysQueryRequest,
    store::{Changes, DeviceChanges, IdentityChanges, Result as StoreResult, Store},
};

#[derive(Debug, Clone)]
pub(crate) struct IdentityManager {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceIdBox>,
    store: Store,
}

impl IdentityManager {
    const MAX_KEY_QUERY_USERS: usize = 250;

    pub fn new(user_id: Arc<UserId>, device_id: Arc<DeviceIdBox>, store: Store) -> Self {
        IdentityManager {
            user_id,
            device_id,
            store,
        }
    }

    fn user_id(&self) -> &UserId {
        &self.user_id
    }

    fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// Receive a successful keys query response.
    ///
    /// Returns a list of devices newly discovered devices and devices that
    /// changed.
    ///
    /// # Arguments
    ///
    /// * `response` - The keys query response of the request that the client
    /// performed.
    pub async fn receive_keys_query_response(
        &self,
        response: &KeysQueryResponse,
    ) -> OlmResult<(DeviceChanges, IdentityChanges)> {
        let changed_devices = self
            .handle_devices_from_key_query(&response.device_keys)
            .await?;
        let changed_identities = self.handle_cross_singing_keys(response).await?;

        let changes = Changes {
            identities: changed_identities.clone(),
            devices: changed_devices.clone(),
            ..Default::default()
        };

        self.store.save_changes(changes).await?;

        Ok((changed_devices, changed_identities))
    }

    /// Handle the device keys part of a key query response.
    ///
    /// # Arguments
    ///
    /// * `device_keys_map` - A map holding the device keys of the users for
    /// which the key query was done.
    ///
    /// Returns a list of devices that changed. Changed here means either
    /// they are new, one of their properties has changed or they got deleted.
    async fn handle_devices_from_key_query(
        &self,
        device_keys_map: &BTreeMap<UserId, BTreeMap<DeviceIdBox, DeviceKeys>>,
    ) -> StoreResult<DeviceChanges> {
        let mut changes = DeviceChanges::default();

        for (user_id, device_map) in device_keys_map {
            // TODO move this out into the handle keys query response method
            // since we might fail handle the new device at any point here or
            // when updating the user identities.
            self.store.update_tracked_user(user_id, false).await?;

            for (device_id, device_keys) in device_map.iter() {
                // We don't need our own device in the device store.
                if user_id == self.user_id() && &**device_id == self.device_id() {
                    continue;
                }

                if user_id != &device_keys.user_id || device_id != &device_keys.device_id {
                    warn!(
                        "Mismatch in device keys payload of device {}|{} from user {}|{}",
                        device_id, device_keys.device_id, user_id, device_keys.user_id
                    );
                    continue;
                }

                let device = self.store.get_readonly_device(&user_id, device_id).await?;

                if let Some(mut device) = device {
                    if let Err(e) = device.update_device(device_keys) {
                        warn!(
                            "Failed to update the device keys for {} {}: {:?}",
                            user_id, device_id, e
                        );
                        continue;
                    }
                    changes.changed.push(device);
                } else {
                    let device = match ReadOnlyDevice::try_from(device_keys) {
                        Ok(d) => d,
                        Err(e) => {
                            warn!(
                                "Failed to create a new device for {} {}: {:?}",
                                user_id, device_id, e
                            );
                            continue;
                        }
                    };
                    info!("Adding a new device to the device store {:?}", device);
                    changes.new.push(device);
                }
            }

            let current_devices: HashSet<&DeviceIdBox> = device_map.keys().collect();
            let stored_devices = self.store.get_readonly_devices(&user_id).await?;
            let stored_devices_set: HashSet<&DeviceIdBox> = stored_devices.keys().collect();

            let deleted_devices_set = stored_devices_set.difference(&current_devices);

            for device_id in deleted_devices_set {
                if let Some(device) = stored_devices.get(*device_id) {
                    device.mark_as_deleted();
                    changes.deleted.push(device.clone());
                }
            }
        }

        Ok(changes)
    }

    /// Handle the device keys part of a key query response.
    ///
    /// # Arguments
    ///
    /// * `response` - The keys query response.
    ///
    /// Returns a list of identities that changed. Changed here means either
    /// they are new, one of their properties has changed or they got deleted.
    async fn handle_cross_singing_keys(
        &self,
        response: &KeysQueryResponse,
    ) -> StoreResult<IdentityChanges> {
        let mut changes = IdentityChanges::default();

        for (user_id, master_key) in &response.master_keys {
            let master_key = MasterPubkey::from(master_key);

            let self_signing = if let Some(s) = response.self_signing_keys.get(user_id) {
                SelfSigningPubkey::from(s)
            } else {
                warn!(
                    "User identity for user {} didn't contain a self signing pubkey",
                    user_id
                );
                continue;
            };

            let result = if let Some(mut i) = self.store.get_user_identity(user_id).await? {
                match &mut i {
                    UserIdentities::Own(ref mut identity) => {
                        let user_signing = if let Some(s) = response.user_signing_keys.get(user_id)
                        {
                            UserSigningPubkey::from(s)
                        } else {
                            warn!(
                                "User identity for our own user {} didn't \
                                  contain a user signing pubkey",
                                user_id
                            );
                            continue;
                        };

                        identity
                            .update(master_key, self_signing, user_signing)
                            .map(|_| (i, false))
                    }
                    UserIdentities::Other(ref mut identity) => identity
                        .update(master_key, self_signing)
                        .map(|_| (i, false)),
                }
            } else if user_id == self.user_id() {
                if let Some(s) = response.user_signing_keys.get(user_id) {
                    let user_signing = UserSigningPubkey::from(s);

                    if master_key.user_id() != user_id
                        || self_signing.user_id() != user_id
                        || user_signing.user_id() != user_id
                    {
                        warn!(
                            "User id mismatch in one of the cross signing keys for user {}",
                            user_id
                        );
                        continue;
                    }

                    OwnUserIdentity::new(master_key, self_signing, user_signing)
                        .map(|i| (UserIdentities::Own(i), true))
                } else {
                    warn!(
                        "User identity for our own user {} didn't contain a \
                        user signing pubkey",
                        user_id
                    );
                    continue;
                }
            } else if master_key.user_id() != user_id || self_signing.user_id() != user_id {
                warn!(
                    "User id mismatch in one of the cross signing keys for user {}",
                    user_id
                );
                continue;
            } else {
                UserIdentity::new(master_key, self_signing)
                    .map(|i| (UserIdentities::Other(i), true))
            };

            match result {
                Ok((i, new)) => {
                    trace!(
                        "Updated or created new user identity for {}: {:?}",
                        user_id,
                        i
                    );
                    if new {
                        changes.new.push(i);
                    } else {
                        changes.changed.push(i);
                    }
                }
                Err(e) => {
                    warn!(
                        "Couldn't update or create new user identity for {}: {:?}",
                        user_id, e
                    );
                    continue;
                }
            }
        }

        Ok(changes)
    }

    /// Get a key query request if one is needed.
    ///
    /// Returns a key query reqeust if the client should query E2E keys,
    /// otherwise None.
    ///
    /// The response of a successful key query requests needs to be passed to
    /// the [`OlmMachine`] with the [`receive_keys_query_response`].
    ///
    /// [`OlmMachine`]: struct.OlmMachine.html
    /// [`receive_keys_query_response`]: #method.receive_keys_query_response
    pub async fn users_for_key_query(&self) -> Vec<KeysQueryRequest> {
        let users = self.store.users_for_key_query();

        if users.is_empty() {
            Vec::new()
        } else {
            let users: Vec<UserId> = users.into_iter().collect();

            users
                .chunks(Self::MAX_KEY_QUERY_USERS)
                .map(|u| u.iter().map(|u| (u.clone(), Vec::new())).collect())
                .map(KeysQueryRequest::new)
                .collect()
        }
    }

    /// Mark that the given user has changed his devices.
    ///
    /// This will queue up the given user for a key query.
    ///
    /// Note: The user already needs to be tracked for it to be queued up for a
    /// key query.
    ///
    /// Returns true if the user was queued up for a key query, false otherwise.
    pub async fn mark_user_as_changed(&self, user_id: &UserId) -> StoreResult<bool> {
        if self.store.is_user_tracked(user_id) {
            self.store.update_tracked_user(user_id, true).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Update the tracked users.
    ///
    /// # Arguments
    ///
    /// * `users` - An iterator over user ids that should be marked for
    /// tracking.
    ///
    /// This will mark users that weren't seen before for a key query and
    /// tracking.
    ///
    /// If the user is already known to the Olm machine it will not be
    /// considered for a key query.
    pub async fn update_tracked_users(&self, users: impl IntoIterator<Item = &UserId>) {
        for user in users {
            if self.store.is_user_tracked(user) {
                continue;
            }

            if let Err(e) = self.store.update_tracked_user(user, true).await {
                warn!("Error storing users for tracking {}", e);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test {
    use std::{convert::TryFrom, sync::Arc};

    use matrix_sdk_common::{
        api::r0::keys::get_keys::Response as KeyQueryResponse,
        identifiers::{user_id, DeviceIdBox, UserId},
        locks::Mutex,
    };

    use matrix_sdk_test::async_test;

    use serde_json::json;

    use crate::{
        identities::IdentityManager,
        machine::test::response_from_file,
        olm::{PrivateCrossSigningIdentity, ReadOnlyAccount},
        store::{CryptoStore, MemoryStore, Store},
        verification::VerificationMachine,
    };

    fn user_id() -> UserId {
        user_id!("@example:localhost")
    }

    fn other_user_id() -> UserId {
        user_id!("@example2:localhost")
    }

    fn device_id() -> DeviceIdBox {
        "WSKKLTJZCL".into()
    }

    fn manager() -> IdentityManager {
        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(user_id())));
        let user_id = Arc::new(user_id());
        let account = ReadOnlyAccount::new(&user_id, &device_id());
        let store: Arc<Box<dyn CryptoStore>> = Arc::new(Box::new(MemoryStore::new()));
        let verification = VerificationMachine::new(account, identity.clone(), store);
        let store = Store::new(
            user_id.clone(),
            identity,
            Arc::new(Box::new(MemoryStore::new())),
            verification,
        );
        IdentityManager::new(user_id, Arc::new(device_id()), store)
    }

    pub(crate) fn other_key_query() -> KeyQueryResponse {
        let data = response_from_file(&json!({
            "device_keys": {
                "@example2:localhost": {
                    "SKISMLNIMH": {
                        "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                        "device_id": "SKISMLNIMH",
                        "keys": {
                            "curve25519:SKISMLNIMH": "qO9xFazIcW8dE0oqHGMojGgJwbBpMOhGnIfJy2pzvmI",
                            "ed25519:SKISMLNIMH": "y3wV3AoyIGREqrJJVH8DkQtlwHBUxoZ9ApP76kFgXQ8"
                        },
                        "signatures": {
                            "@example2:localhost": {
                                "ed25519:SKISMLNIMH": "YwbT35rbjKoYFZVU1tQP8MsL06+znVNhNzUMPt6jTEYRBFoC4GDq9hQEJBiFSq37r1jvLMteggVAWw37fs1yBA",
                                "ed25519:ZtFrSkJ1qB8Jph/ql9Eo/lKpIYCzwvKAKXfkaS4XZNc": "PWuuTE/aTkp1EJQkPHhRx2BxbF+wjMIDFxDRp7JAerlMkDsNFUTfRRusl6vqROPU36cl+yY8oeJTZGFkU6+pBQ"
                            }
                        },
                        "user_id": "@example2:localhost",
                        "unsigned": {
                            "device_display_name": "Riot Desktop (Linux)"
                        }
                    }
                }
            },
            "failures": {},
            "master_keys": {
                "@example2:localhost": {
                    "user_id": "@example2:localhost",
                    "usage": ["master"],
                    "keys": {
                        "ed25519:kC/HmRYw4HNqUp/i4BkwYENrf+hd9tvdB7A1YOf5+Do": "kC/HmRYw4HNqUp/i4BkwYENrf+hd9tvdB7A1YOf5+Do"
                    },
                    "signatures": {
                        "@example2:localhost": {
                            "ed25519:SKISMLNIMH": "KdUZqzt8VScGNtufuQ8lOf25byYLWIhmUYpPENdmM8nsldexD7vj+Sxoo7PknnTX/BL9h2N7uBq0JuykjunCAw"
                        }
                    }
                }
            },
            "self_signing_keys": {
                "@example2:localhost": {
                    "user_id": "@example2:localhost",
                    "usage": ["self_signing"],
                    "keys": {
                        "ed25519:ZtFrSkJ1qB8Jph/ql9Eo/lKpIYCzwvKAKXfkaS4XZNc": "ZtFrSkJ1qB8Jph/ql9Eo/lKpIYCzwvKAKXfkaS4XZNc"
                    },
                    "signatures": {
                        "@example2:localhost": {
                            "ed25519:kC/HmRYw4HNqUp/i4BkwYENrf+hd9tvdB7A1YOf5+Do": "W/O8BnmiUETPpH02mwYaBgvvgF/atXnusmpSTJZeUSH/vHg66xiZOhveQDG4cwaW8iMa+t9N4h1DWnRoHB4mCQ"
                        }
                    }
                }
            },
            "user_signing_keys": {}
        }));
        KeyQueryResponse::try_from(data).expect("Can't parse the keys upload response")
    }

    pub(crate) fn own_key_query() -> KeyQueryResponse {
        let data = response_from_file(&json!({
          "device_keys": {
            "@example:localhost": {
              "WSKKLTJZCL": {
                "algorithms": [
                  "m.olm.v1.curve25519-aes-sha2",
                  "m.megolm.v1.aes-sha2"
                ],
                "device_id": "WSKKLTJZCL",
                "keys": {
                  "curve25519:WSKKLTJZCL": "wnip2tbJBJxrFayC88NNJpm61TeSNgYcqBH4T9yEDhU",
                  "ed25519:WSKKLTJZCL": "lQ+eshkhgKoo+qp9Qgnj3OX5PBoWMU5M9zbuEevwYqE"
                },
                "signatures": {
                  "@example:localhost": {
                    "ed25519:WSKKLTJZCL": "SKpIUnq7QK0xleav0PrIQyKjVm+TgZr7Yi8cKjLeZDtkgyToE2d4/e3Aj79dqOlLB92jFVE4d1cM/Ry04wFwCA",
                    "ed25519:0C8lCBxrvrv/O7BQfsKnkYogHZX3zAgw3RfJuyiq210": "9UGu1iC5YhFCdELGfB29YaV+QE0t/X5UDSsPf4QcdZyXIwyp9zBbHX2lh9vWudNQ+akZpaq7ZRaaM+4TCnw/Ag"
                  }
                },
                "user_id": "@example:localhost",
                "unsigned": {
                  "device_display_name": "Cross signing capable"
                }
              },
              "LVWOVGOXME": {
                "algorithms": [
                  "m.olm.v1.curve25519-aes-sha2",
                  "m.megolm.v1.aes-sha2"
                ],
                "device_id": "LVWOVGOXME",
                "keys": {
                  "curve25519:LVWOVGOXME": "KMfWKUhnDW1D11hNzATs/Ax1FQRsJxKCWzq0NyGtIiI",
                  "ed25519:LVWOVGOXME": "k+NC3L7CBD6fBClcHBrKLOkqCyGNSKhWXiH5Q2STRnA"
                },
                "signatures": {
                  "@example:localhost": {
                    "ed25519:LVWOVGOXME": "39Ir5Bttpc5+bQwzLj7rkjm5E5/cp/JTbMJ/t0enj6J5w9MXVBFOUqqM2hpaRaRwILMMpwYbJ8IOGjl0Y/MGAw"
                  }
                },
                "user_id": "@example:localhost",
                "unsigned": {
                  "device_display_name": "Non-cross signing"
                }
              }
            }
          },
          "failures": {},
          "master_keys": {
            "@example:localhost": {
              "user_id": "@example:localhost",
              "usage": [
                "master"
              ],
              "keys": {
                "ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0": "rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0"
              },
              "signatures": {
                "@example:localhost": {
                  "ed25519:WSKKLTJZCL": "ZzJp1wtmRdykXAUEItEjNiFlBrxx8L6/Vaen9am8AuGwlxxJtOkuY4m+4MPLvDPOgavKHLsrRuNLAfCeakMlCQ"
                }
              }
            }
          },
          "self_signing_keys": {
            "@example:localhost": {
              "user_id": "@example:localhost",
              "usage": [
                "self_signing"
              ],
              "keys": {
                "ed25519:0C8lCBxrvrv/O7BQfsKnkYogHZX3zAgw3RfJuyiq210": "0C8lCBxrvrv/O7BQfsKnkYogHZX3zAgw3RfJuyiq210"
              },
              "signatures": {
                "@example:localhost": {
                  "ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0": "AC7oDUW4rUhtInwb4lAoBJ0wAuu4a5k+8e34B5+NKsDB8HXRwgVwUWN/MRWc/sJgtSbVlhzqS9THEmQQ1C51Bw"
                }
              }
            }
          },
          "user_signing_keys": {
            "@example:localhost": {
              "user_id": "@example:localhost",
              "usage": [
                "user_signing"
              ],
              "keys": {
                "ed25519:DU9z4gBFKFKCk7a13sW9wjT0Iyg7Hqv5f0BPM7DEhPo": "DU9z4gBFKFKCk7a13sW9wjT0Iyg7Hqv5f0BPM7DEhPo"
              },
              "signatures": {
                "@example:localhost": {
                  "ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0": "C4L2sx9frGqj8w41KyynHGqwUbbwBYRZpYCB+6QWnvQFA5Oi/1PJj8w5anwzEsoO0TWmLYmf7FXuAGewanOWDg"
                }
              }
            }
          }
        }));
        KeyQueryResponse::try_from(data).expect("Can't parse the keys upload response")
    }

    #[async_test]
    async fn test_manager_creation() {
        let manager = manager();
        assert!(manager.users_for_key_query().await.is_empty())
    }

    #[async_test]
    async fn test_manager_key_query_response() {
        let manager = manager();
        let other_user = other_user_id();
        let devices = manager.store.get_user_devices(&other_user).await.unwrap();
        assert_eq!(devices.devices().count(), 0);

        manager
            .receive_keys_query_response(&other_key_query())
            .await
            .unwrap();

        let devices = manager.store.get_user_devices(&other_user).await.unwrap();
        assert_eq!(devices.devices().count(), 1);

        let device = manager
            .store
            .get_readonly_device(&other_user, "SKISMLNIMH".into())
            .await
            .unwrap()
            .unwrap();
        let identity = manager
            .store
            .get_user_identity(&other_user)
            .await
            .unwrap()
            .unwrap();
        let identity = identity.other().unwrap();

        assert!(identity.is_device_signed(&device).is_ok())
    }

    #[async_test]
    async fn test_manager_own_key_query_response() {
        let manager = manager();
        let other_user = other_user_id();
        let devices = manager.store.get_user_devices(&other_user).await.unwrap();
        assert_eq!(devices.devices().count(), 0);

        manager
            .receive_keys_query_response(&other_key_query())
            .await
            .unwrap();

        let devices = manager.store.get_user_devices(&other_user).await.unwrap();
        assert_eq!(devices.devices().count(), 1);

        let device = manager
            .store
            .get_readonly_device(&other_user, "SKISMLNIMH".into())
            .await
            .unwrap()
            .unwrap();
        let identity = manager
            .store
            .get_user_identity(&other_user)
            .await
            .unwrap()
            .unwrap();
        let identity = identity.other().unwrap();

        assert!(identity.is_device_signed(&device).is_ok())
    }
}
