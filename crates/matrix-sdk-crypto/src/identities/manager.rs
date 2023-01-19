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
    collections::{BTreeMap, BTreeSet, HashSet},
    ops::Deref,
    sync::Arc,
    time::Duration,
};

use futures_util::future::join_all;
use matrix_sdk_common::{
    executor::spawn,
    timeout::{timeout, ElapsedError},
};
use ruma::{
    api::client::keys::get_keys::v3::Response as KeysQueryResponse, serde::Raw, DeviceId,
    OwnedDeviceId, OwnedServerName, OwnedUserId, ServerName, UserId,
};
use tracing::{debug, info, trace, warn};

use crate::{
    error::OlmResult,
    identities::{
        MasterPubkey, ReadOnlyDevice, ReadOnlyOwnUserIdentity, ReadOnlyUserIdentities,
        ReadOnlyUserIdentity, SelfSigningPubkey, UserSigningPubkey,
    },
    olm::PrivateCrossSigningIdentity,
    requests::KeysQueryRequest,
    store::{Changes, DeviceChanges, IdentityChanges, Result as StoreResult, Store},
    types::{CrossSigningKey, DeviceKeys},
    utilities::FailuresCache,
    LocalTrust,
};

enum DeviceChange {
    New(ReadOnlyDevice),
    Updated(ReadOnlyDevice),
    None,
}

/// A listener that can notify if a `/keys/query` response has been received.
#[derive(Clone, Debug)]
pub(crate) struct KeysQueryListener {
    inner: Arc<event_listener::Event>,
    store: Store,
}

/// Result type telling us if a `/keys/query` response was expected for a given
/// user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserKeyQueryResult {
    WasPending,
    WasNotPending,
}

impl KeysQueryListener {
    pub(crate) fn new(store: Store) -> Self {
        Self { inner: event_listener::Event::new().into(), store }
    }

    /// Notify our listeners that we received a `/keys/query` response.
    fn notify(&self) {
        self.inner.notify(usize::MAX);
    }

    /// Wait for a `/keys/query` response to be received if one is expected for
    /// the given user.
    ///
    /// If the given timeout has elapsed the method will stop waiting and return
    /// an error.
    pub async fn wait_if_user_pending(
        &self,
        timeout: Duration,
        user: &UserId,
    ) -> Result<UserKeyQueryResult, ElapsedError> {
        let users_for_key_query = self.store.users_for_key_query().await.unwrap_or_default();

        if users_for_key_query.contains(user) {
            if let Err(e) = self.wait(timeout).await {
                warn!(
                    user_id = ?user,
                    "The user has a pending `/key/query` request which did \
                    not finish yet, some devices might be missing."
                );

                Err(e)
            } else {
                Ok(UserKeyQueryResult::WasPending)
            }
        } else {
            Ok(UserKeyQueryResult::WasNotPending)
        }
    }

    /// Wait for a `/keys/query` response to be received.
    ///
    /// If the given timeout has elapsed the method will stop waiting and return
    /// an error.
    pub async fn wait(&self, duration: Duration) -> Result<(), ElapsedError> {
        timeout(self.inner.listen(), duration).await
    }
}

#[derive(Debug, Clone)]
pub(crate) struct IdentityManager {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    keys_query_listener: KeysQueryListener,
    failures: FailuresCache<OwnedServerName>,
    store: Store,
}

impl IdentityManager {
    const MAX_KEY_QUERY_USERS: usize = 250;

    pub fn new(user_id: Arc<UserId>, device_id: Arc<DeviceId>, store: Store) -> Self {
        let keys_query_listener = KeysQueryListener::new(store.clone());

        IdentityManager {
            user_id,
            device_id,
            store,
            keys_query_listener,
            failures: Default::default(),
        }
    }

    fn user_id(&self) -> &UserId {
        &self.user_id
    }

    pub fn listen_for_received_queries(&self) -> KeysQueryListener {
        self.keys_query_listener.clone()
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
        debug!(
            users = ?response.device_keys.keys().collect::<BTreeSet<_>>(),
            failures = ?response.failures,
            "Handling a keys query response"
        );

        // Parse the strings into server names and filter out our own server. We should
        // never get failures from our own server but let's remove it as a
        // precaution anyways.
        let failed_servers = response
            .failures
            .keys()
            .filter_map(|k| ServerName::parse(k).ok())
            .filter(|s| s != self.user_id().server_name());
        let successful_servers = response.device_keys.keys().map(|u| u.server_name());

        // Append the new failed servers and remove any successful servers. We
        // need to explicitly remove the successful servers because the cache
        // doesn't automatically remove entries that elapse. Instead, the effect
        // is that elapsed servers will be retried and their delays incremented.
        self.failures.extend(failed_servers);
        self.failures.remove(successful_servers);

        let devices = self.handle_devices_from_key_query(response.device_keys.clone()).await?;
        let (identities, cross_signing_identity) = self.handle_cross_singing_keys(response).await?;

        let changes = Changes {
            identities: identities.clone(),
            devices: devices.clone(),
            private_identity: cross_signing_identity,
            ..Default::default()
        };

        // TODO: turn this into a single transaction.
        self.store.save_changes(changes).await?;
        let updated_users: Vec<&UserId> = response.device_keys.keys().map(Deref::deref).collect();

        for user_id in updated_users {
            self.store.update_tracked_user(user_id, false).await?;
        }

        let changed_devices = devices.changed.iter().fold(BTreeMap::new(), |mut acc, d| {
            acc.entry(d.user_id()).or_insert_with(BTreeSet::new).insert(d.device_id());
            acc
        });

        let new_devices = devices.new.iter().fold(BTreeMap::new(), |mut acc, d| {
            acc.entry(d.user_id()).or_insert_with(BTreeSet::new).insert(d.device_id());
            acc
        });

        let deleted_devices = devices.deleted.iter().fold(BTreeMap::new(), |mut acc, d| {
            acc.entry(d.user_id()).or_insert_with(BTreeSet::new).insert(d.device_id());
            acc
        });

        let new_identities = identities.new.iter().map(|i| i.user_id()).collect::<BTreeSet<_>>();
        let changed_identities =
            identities.changed.iter().map(|i| i.user_id()).collect::<BTreeSet<_>>();

        debug!(
            ?new_devices,
            ?changed_devices,
            ?deleted_devices,
            ?new_identities,
            ?changed_identities,
            "Finished handling of the keys/query response"
        );

        self.keys_query_listener.notify();

        Ok((devices, identities))
    }

    async fn update_or_create_device(
        store: Store,
        device_keys: DeviceKeys,
    ) -> StoreResult<DeviceChange> {
        let old_device =
            store.get_readonly_device(&device_keys.user_id, &device_keys.device_id).await?;

        if let Some(mut device) = old_device {
            if let Err(e) = device.update_device(&device_keys) {
                warn!(
                    user_id = device.user_id().as_str(),
                    device_id = device.device_id().as_str(),
                    error = ?e,
                    "Failed to update device keys",
                );

                Ok(DeviceChange::None)
            } else {
                Ok(DeviceChange::Updated(device))
            }
        } else {
            match ReadOnlyDevice::try_from(&device_keys) {
                Ok(d) => {
                    // If this is our own device, check that the server isn't
                    // lying about our keys, also mark the device as locally
                    // trusted.
                    if d.user_id() == store.user_id() && d.device_id() == store.device_id() {
                        let local_device_keys = store.account().unsigned_device_keys();

                        if d.keys() == &local_device_keys.keys {
                            d.set_trust_state(LocalTrust::Verified);

                            trace!(
                                user_id = d.user_id().as_str(),
                                device_id = d.device_id().as_str(),
                                keys = ?d.keys(),
                                "Adding our own device to the device store, \
                                marking it as locally verified",
                            );

                            Ok(DeviceChange::New(d))
                        } else {
                            Ok(DeviceChange::None)
                        }
                    } else {
                        trace!(
                            user_id = d.user_id().as_str(),
                            device_id = d.device_id().as_str(),
                            keys = ?d.keys(),
                            "Adding a new device to the device store",
                        );

                        Ok(DeviceChange::New(d))
                    }
                }
                Err(e) => {
                    warn!(
                        user_id = device_keys.user_id.as_str(),
                        device_id = device_keys.device_id.as_str(),
                        error = ?e,
                        "Failed to create a new device",
                    );

                    Ok(DeviceChange::None)
                }
            }
        }
    }

    async fn update_user_devices(
        store: Store,
        own_user_id: Arc<UserId>,
        own_device_id: Arc<DeviceId>,
        user_id: OwnedUserId,
        device_map: BTreeMap<OwnedDeviceId, Raw<ruma::encryption::DeviceKeys>>,
    ) -> StoreResult<DeviceChanges> {
        let own_device_id = (*own_device_id).to_owned();

        let mut changes = DeviceChanges::default();

        let current_devices: HashSet<OwnedDeviceId> = device_map.keys().cloned().collect();

        let tasks = device_map.into_iter().filter_map(|(device_id, device_keys)| match device_keys
            .deserialize_as::<DeviceKeys>(
        ) {
            Ok(device_keys) => {
                if user_id != device_keys.user_id || device_id != device_keys.device_id {
                    warn!(
                        user_id = user_id.as_str(),
                        device_id = device_id.as_str(),
                        device_key_user = device_keys.user_id.as_str(),
                        device_key_device_id = device_keys.device_id.as_str(),
                        "Mismatch in the device keys payload",
                    );
                    None
                } else {
                    Some(spawn(Self::update_or_create_device(store.clone(), device_keys)))
                }
            }
            Err(e) => {
                warn!(
                    user_id = user_id.as_str(),
                    device_id = device_id.as_str(),
                    error = ?e,
                    "Device keys failed to deserialize",
                );
                None
            }
        });

        let results = join_all(tasks).await;

        for device in results {
            let device = device.expect("Creating or updating a device panicked")?;

            match device {
                DeviceChange::New(d) => changes.new.push(d),
                DeviceChange::Updated(d) => changes.changed.push(d),
                DeviceChange::None => (),
            }
        }

        let current_devices: HashSet<&OwnedDeviceId> = current_devices.iter().collect();
        let stored_devices = store.get_readonly_devices_unfiltered(&user_id).await?;
        let stored_devices_set: HashSet<&OwnedDeviceId> = stored_devices.keys().collect();
        let deleted_devices_set = stored_devices_set.difference(&current_devices);

        for device_id in deleted_devices_set {
            if user_id == *own_user_id && *device_id == &own_device_id {
                let identity_keys = store.account().identity_keys();

                warn!(
                    user_id = own_user_id.as_str(),
                    device_id = own_device_id.as_str(),
                    curve25519_key = ?identity_keys.curve25519,
                    ed25519_key = ?identity_keys.ed25519,
                    "Our own device might have been deleted"
                );
            } else if let Some(device) = stored_devices.get(*device_id) {
                device.mark_as_deleted();
                changes.deleted.push(device.clone());
            }
        }

        Ok(changes)
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
        device_keys_map: BTreeMap<
            OwnedUserId,
            BTreeMap<OwnedDeviceId, Raw<ruma::encryption::DeviceKeys>>,
        >,
    ) -> StoreResult<DeviceChanges> {
        let mut changes = DeviceChanges::default();

        let tasks = device_keys_map.into_iter().map(|(user_id, device_keys_map)| {
            spawn(Self::update_user_devices(
                self.store.clone(),
                self.user_id.clone(),
                self.device_id.clone(),
                user_id,
                device_keys_map,
            ))
        });

        let results = join_all(tasks).await;

        for result in results {
            let change_fragment = result.expect("Panic while updating user devices")?;

            changes.extend(change_fragment);
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
    ) -> StoreResult<(IdentityChanges, Option<PrivateCrossSigningIdentity>)> {
        let mut changes = IdentityChanges::default();
        let mut changed_identity = None;

        // TODO: this is a bit chunky, refactor this into smaller methods.

        for (user_id, master_key) in &response.master_keys {
            match master_key.deserialize_as::<CrossSigningKey>() {
                Ok(master_key) => {
                    let master_key = MasterPubkey::from(master_key);

                    let self_signing = if let Some(s) = response
                        .self_signing_keys
                        .get(user_id)
                        .and_then(|k| k.deserialize_as::<CrossSigningKey>().ok())
                    {
                        SelfSigningPubkey::from(s)
                    } else {
                        warn!(
                            user_id = user_id.as_str(),
                            "A user identity didn't contain a self signing pubkey \
                            or the key was invalid"
                        );
                        continue;
                    };

                    let result = if let Some(mut i) = self.store.get_user_identity(user_id).await? {
                        match &mut i {
                            ReadOnlyUserIdentities::Own(identity) => {
                                let user_signing = if let Some(s) = response
                                    .user_signing_keys
                                    .get(user_id)
                                    .and_then(|k| k.deserialize_as::<CrossSigningKey>().ok())
                                {
                                    UserSigningPubkey::from(s)
                                } else {
                                    warn!(
                                        user_id = user_id.as_str(),
                                        "User identity for our own user didn't \
                                        contain a user signing pubkey",
                                    );
                                    continue;
                                };

                                identity
                                    .update(master_key, self_signing, user_signing)
                                    .map(|_| (i, false))
                            }
                            ReadOnlyUserIdentities::Other(identity) => {
                                identity.update(master_key, self_signing).map(|_| (i, false))
                            }
                        }
                    } else if user_id == self.user_id() {
                        if let Some(s) = response
                            .user_signing_keys
                            .get(user_id)
                            .and_then(|k| k.deserialize_as::<CrossSigningKey>().ok())
                        {
                            let user_signing = UserSigningPubkey::from(s);

                            if master_key.user_id() != user_id
                                || self_signing.user_id() != user_id
                                || user_signing.user_id() != user_id
                            {
                                warn!(
                                    user_id = user_id.as_str(),
                                    "User ID mismatch in one of the cross signing keys",
                                );
                                continue;
                            }

                            ReadOnlyOwnUserIdentity::new(master_key, self_signing, user_signing)
                                .map(|i| (ReadOnlyUserIdentities::Own(i), true))
                        } else {
                            warn!(
                                user_id = user_id.as_str(),
                                "User identity for our own user didn't contain a \
                                user signing pubkey or the key isn't valid",
                            );
                            continue;
                        }
                    } else if master_key.user_id() != user_id || self_signing.user_id() != user_id {
                        warn!(
                            user = user_id.as_str(),
                            "User ID mismatch in one of the cross signing keys",
                        );
                        continue;
                    } else {
                        ReadOnlyUserIdentity::new(master_key, self_signing)
                            .map(|i| (ReadOnlyUserIdentities::Other(i), true))
                    };

                    match result {
                        Ok((i, new)) => {
                            if let Some(identity) = i.own() {
                                let private_identity = self.store.private_identity();
                                let private_identity = private_identity.lock().await;

                                let result = private_identity.clear_if_differs(identity).await;

                                if result.any_cleared() {
                                    changed_identity = Some((*private_identity).clone());
                                    info!(cleared = ?result, "Removed some or all of our private cross signing keys");
                                } else if new && private_identity.has_master_key().await {
                                    // If the master key didn't rotate above (`clear_if_differs`),
                                    // then this means that the public part and the private parts of
                                    // the master key match. We previously did a signature check, so
                                    // this means that the private part of the master key has signed
                                    // the identity. We can safely mark the public part of the
                                    // identity as verified.
                                    identity.mark_as_verified();
                                    trace!("Received our own user identity, for which we possess the private key. Marking as verified.");
                                }
                            }

                            if new {
                                trace!(user_id = user_id.as_str(), identity = ?i, "Created new user identity");
                                changes.new.push(i);
                            } else {
                                trace!(user_id = user_id.as_str(), identity = ?i, "Updated a user identity");
                                changes.changed.push(i);
                            }
                        }
                        Err(e) => {
                            warn!(
                                user_id = user_id.as_str(),
                                error = ?e,
                                "Couldn't update or create new user identity"
                            );
                            continue;
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        user_id = user_id.as_str(),
                        error = ?e,
                        "Couldn't update or create new user identity"
                    );
                    continue;
                }
            }
        }

        Ok((changes, changed_identity))
    }

    /// Get a key query request if one is needed.
    ///
    /// Returns a key query request if the client should query E2E keys,
    /// otherwise None.
    ///
    /// The response of a successful key query requests needs to be passed to
    /// the [`OlmMachine`] with the [`receive_keys_query_response`].
    ///
    /// [`OlmMachine`]: struct.OlmMachine.html
    /// [`receive_keys_query_response`]: #method.receive_keys_query_response
    pub async fn users_for_key_query(&self) -> StoreResult<Vec<KeysQueryRequest>> {
        let users = self.store.users_for_key_query().await?;

        // We always want to track our own user, but in case we aren't in an encrypted
        // room yet, we won't be tracking ourselves yet. This ensures we are always
        // tracking ourselves.
        //
        // The check for emptiness is done first for performance.
        let users =
            if users.is_empty() && !self.store.tracked_users().await?.contains(self.user_id()) {
                self.update_tracked_users([self.user_id()]).await?;
                self.store.users_for_key_query().await?
            } else {
                users
            };

        if users.is_empty() {
            Ok(Vec::new())
        } else {
            let users: Vec<OwnedUserId> =
                users.into_iter().filter(|u| !self.failures.contains(u.server_name())).collect();

            Ok(users
                .chunks(Self::MAX_KEY_QUERY_USERS)
                .map(|u| u.iter().map(|u| (u.clone(), Vec::new())).collect())
                .map(KeysQueryRequest::new)
                .collect())
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
        if self.store.is_user_tracked(user_id).await? {
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
    pub async fn update_tracked_users(
        &self,
        users: impl IntoIterator<Item = &UserId>,
    ) -> StoreResult<()> {
        for user in users {
            if self.store.is_user_tracked(user).await? {
                continue;
            }

            self.store.update_tracked_user(user, true).await?;
        }

        Ok(())
    }
}

#[cfg(any(test, feature = "testing"))]
pub(crate) mod testing {
    #![allow(dead_code)]
    use std::sync::Arc;

    use matrix_sdk_common::locks::Mutex;
    use ruma::{
        api::{client::keys::get_keys::v3::Response as KeyQueryResponse, IncomingResponse},
        device_id, user_id, DeviceId, UserId,
    };
    use serde_json::json;

    use crate::{
        identities::IdentityManager,
        machine::testing::response_from_file,
        olm::{PrivateCrossSigningIdentity, ReadOnlyAccount},
        store::{CryptoStore, MemoryStore, Store},
        types::DeviceKeys,
        verification::VerificationMachine,
        UploadSigningKeysRequest,
    };

    pub fn user_id() -> &'static UserId {
        user_id!("@example:localhost")
    }

    pub fn other_user_id() -> &'static UserId {
        user_id!("@example2:localhost")
    }

    pub fn device_id() -> &'static DeviceId {
        device_id!("WSKKLTJZCL")
    }

    pub(crate) async fn manager() -> IdentityManager {
        let identity = PrivateCrossSigningIdentity::new(user_id().into()).await;
        let identity = Arc::new(Mutex::new(identity));
        let user_id = Arc::from(user_id());
        let account = ReadOnlyAccount::new(&user_id, device_id());
        let store: Arc<dyn CryptoStore> = Arc::new(MemoryStore::new());
        let verification = VerificationMachine::new(account, identity.clone(), store);
        let store =
            Store::new(user_id.clone(), identity, Arc::new(MemoryStore::new()), verification);
        IdentityManager::new(user_id, device_id().into(), store)
    }

    pub fn other_key_query() -> KeyQueryResponse {
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
        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the keys upload response")
    }

    pub fn own_key_query() -> KeyQueryResponse {
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
        KeyQueryResponse::try_from_http_response(data)
            .expect("Can't parse the keys upload response")
    }

    pub fn key_query(
        identity: UploadSigningKeysRequest,
        device_keys: DeviceKeys,
    ) -> KeyQueryResponse {
        let json = json!({
            "device_keys": {
                "@example:localhost": {
                    device_keys.device_id.to_string(): device_keys
                }
            },
            "failures": {},
            "master_keys": {
                "@example:localhost": identity.master_key
            },
            "self_signing_keys": {
                "@example:localhost": identity.self_signing_key
            },
            "user_signing_keys": {
                "@example:localhost": identity.user_signing_key
            },
          }
        );

        KeyQueryResponse::try_from_http_response(response_from_file(&json))
            .expect("Can't parse the keys upload response")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use matrix_sdk_test::{async_test, response_from_file};
    use ruma::{
        api::{client::keys::get_keys::v3::Response as KeysQueryResponse, IncomingResponse},
        device_id, user_id,
    };
    use serde_json::json;

    use super::testing::{device_id, key_query, manager, other_key_query, other_user_id, user_id};

    fn key_query_without_failures() -> KeysQueryResponse {
        let response = json!({
            "device_keys": {
                "@alice:example.org": {
                },
            }
        });

        let response = response_from_file(&response);

        KeysQueryResponse::try_from_http_response(response).unwrap()
    }
    fn key_query_with_failures() -> KeysQueryResponse {
        let response = json!({
            "device_keys": {
            },
            "failures": {
                "example.org": {
                    "errcode": "M_RESOURCE_LIMIT_EXCEEDED",
                    "error": "Not yet ready to retry",
                }
            }
        });

        let response = response_from_file(&response);

        KeysQueryResponse::try_from_http_response(response).unwrap()
    }

    #[async_test]
    async fn test_manager_creation() {
        let manager = manager().await;
        assert!(manager.store.tracked_users().await.unwrap().is_empty())
    }

    #[async_test]
    async fn test_manager_key_query_response() {
        let manager = manager().await;
        let other_user = other_user_id();
        let devices = manager.store.get_user_devices(other_user).await.unwrap();
        assert_eq!(devices.devices().count(), 0);

        let listener = manager.listen_for_received_queries();

        let task = tokio::task::spawn(async move { listener.wait(Duration::from_secs(10)).await });

        manager.receive_keys_query_response(&other_key_query()).await.unwrap();

        task.await.unwrap().unwrap();

        let devices = manager.store.get_user_devices(other_user).await.unwrap();
        assert_eq!(devices.devices().count(), 1);

        let device = manager
            .store
            .get_readonly_device(other_user, device_id!("SKISMLNIMH"))
            .await
            .unwrap()
            .unwrap();
        let identity = manager.store.get_user_identity(other_user).await.unwrap().unwrap();
        let identity = identity.other().unwrap();

        identity.is_device_signed(&device).unwrap();
    }

    #[async_test]
    async fn test_manager_own_key_query_response() {
        let manager = manager().await;
        let our_user = user_id();
        let devices = manager.store.get_user_devices(our_user).await.unwrap();
        assert_eq!(devices.devices().count(), 0);

        let private_identity = manager.store.private_identity();
        let private_identity = private_identity.lock().await;
        let identity_request = private_identity.as_upload_request().await;
        drop(private_identity);

        let device_keys = manager.store.account().device_keys().await;
        manager
            .receive_keys_query_response(&key_query(identity_request, device_keys))
            .await
            .unwrap();

        let identity = manager.store.get_user_identity(our_user).await.unwrap().unwrap();
        let identity = identity.own().unwrap();
        assert!(identity.is_verified());

        let devices = manager.store.get_user_devices(our_user).await.unwrap();
        assert_eq!(devices.devices().count(), 1);

        let device =
            manager.store.get_readonly_device(our_user, device_id!(device_id())).await.unwrap();

        assert!(device.is_some());
    }

    #[async_test]
    async fn no_tracked_users_key_query_request() {
        let manager = manager().await;

        assert!(
            manager.store.tracked_users().await.unwrap().is_empty(),
            "No users are initially tracked"
        );

        let requests = manager.users_for_key_query().await.unwrap();

        assert!(!requests.is_empty(), "We query the keys for our own user");
        assert!(
            manager.store.tracked_users().await.unwrap().contains(manager.user_id()),
            "Our own user is now tracked"
        );
    }

    #[async_test]
    async fn failure_handling() {
        let manager = manager().await;
        let alice = user_id!("@alice:example.org");

        assert!(
            manager.store.tracked_users().await.unwrap().is_empty(),
            "No users are initially tracked"
        );
        manager.store.update_tracked_user(alice, true).await.unwrap();

        assert!(
            manager.store.tracked_users().await.unwrap().contains(alice),
            "Alice is tracked after being marked as tracked"
        );
        assert!(manager
            .users_for_key_query()
            .await
            .unwrap()
            .iter()
            .any(|r| r.device_keys.contains_key(alice)));

        let response = key_query_with_failures();

        manager.receive_keys_query_response(&response).await.unwrap();
        assert!(manager.failures.contains(alice.server_name()));
        assert!(!manager
            .users_for_key_query()
            .await
            .unwrap()
            .iter()
            .any(|r| r.device_keys.contains_key(alice)));

        let response = key_query_without_failures();
        manager.receive_keys_query_response(&response).await.unwrap();
        assert!(!manager.failures.contains(alice.server_name()));
        assert!(!manager
            .users_for_key_query()
            .await
            .unwrap()
            .iter()
            .any(|r| r.device_keys.contains_key(alice)));

        manager.store.update_tracked_user(alice, true).await.unwrap();
        assert!(manager
            .users_for_key_query()
            .await
            .unwrap()
            .iter()
            .any(|r| r.device_keys.contains_key(alice)));
    }
}
