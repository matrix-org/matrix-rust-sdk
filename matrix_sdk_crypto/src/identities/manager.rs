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
    store::{Result as StoreResult, Store},
};

#[derive(Debug, Clone)]
pub(crate) struct IdentityManager {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceIdBox>,
    store: Store,
}

impl IdentityManager {
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
    ) -> OlmResult<(Vec<ReadOnlyDevice>, Vec<UserIdentities>)> {
        // TODO create a enum that tells us how the device/identity changed,
        // e.g. new/deleted/display name change.
        //
        // TODO create a struct that will hold the device/identity and the
        // change enum and return the struct.
        //
        // TODO once outbound group sessions hold on to the set of users that
        // received the session, invalidate the session if a user device
        // got added/deleted.
        let changed_devices = self
            .handle_devices_from_key_query(&response.device_keys)
            .await?;
        self.store.save_devices(&changed_devices).await?;
        let changed_identities = self.handle_cross_singing_keys(response).await?;
        self.store.save_user_identities(&changed_identities).await?;

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
    ) -> StoreResult<Vec<ReadOnlyDevice>> {
        let mut changed_devices = Vec::new();

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
                        "Mismatch in device keys payload of device {} from user {}",
                        device_keys.device_id, device_keys.user_id
                    );
                    continue;
                }

                let device = self.store.get_device(&user_id, device_id).await?;

                let device = if let Some(mut device) = device {
                    if let Err(e) = device.update_device(device_keys) {
                        warn!(
                            "Failed to update the device keys for {} {}: {:?}",
                            user_id, device_id, e
                        );
                        continue;
                    }
                    device
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
                    device
                };

                changed_devices.push(device);
            }

            let current_devices: HashSet<&DeviceId> =
                device_map.keys().map(|id| id.as_ref()).collect();
            let stored_devices = self.store.get_user_devices(&user_id).await.unwrap();
            let stored_devices_set: HashSet<&DeviceId> = stored_devices.keys().collect();

            let deleted_devices = stored_devices_set.difference(&current_devices);

            for device_id in deleted_devices {
                if let Some(device) = stored_devices.get(device_id) {
                    device.mark_as_deleted();
                    self.store.delete_device(device).await?;
                }
            }
        }

        Ok(changed_devices)
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
    ) -> StoreResult<Vec<UserIdentities>> {
        let mut changed = Vec::new();

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

            let identity = if let Some(mut i) = self.store.get_user_identity(user_id).await? {
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
                            .map(|_| i)
                    }
                    UserIdentities::Other(ref mut identity) => {
                        identity.update(master_key, self_signing).map(|_| i)
                    }
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
                        .map(UserIdentities::Own)
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
                UserIdentity::new(master_key, self_signing).map(UserIdentities::Other)
            };

            match identity {
                Ok(i) => {
                    trace!(
                        "Updated or created new user identity for {}: {:?}",
                        user_id,
                        i
                    );
                    changed.push(i);
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

        Ok(changed)
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
    pub async fn users_for_key_query(&self) -> Option<KeysQueryRequest> {
        let mut users = self.store.users_for_key_query();

        if users.is_empty() {
            None
        } else {
            let mut device_keys: BTreeMap<UserId, Vec<Box<DeviceId>>> = BTreeMap::new();

            for user in users.drain() {
                device_keys.insert(user, Vec::new());
            }

            Some(KeysQueryRequest::new(device_keys))
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
