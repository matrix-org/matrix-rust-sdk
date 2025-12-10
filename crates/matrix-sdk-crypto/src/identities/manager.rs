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
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ops::Deref,
    sync::Arc,
    time::Duration,
};

use futures_util::future::join_all;
use itertools::Itertools;
use matrix_sdk_common::{executor::spawn, failures_cache::FailuresCache};
use ruma::{
    OwnedDeviceId, OwnedServerName, OwnedTransactionId, OwnedUserId, ServerName, TransactionId,
    UserId, api::client::keys::get_keys::v3::Response as KeysQueryResponse, serde::Raw,
};
use tokio::sync::Mutex;
use tracing::{Level, debug, enabled, info, instrument, trace, warn};

use crate::{
    CryptoStoreError, LocalTrust, OwnUserIdentity, SignatureError, UserIdentity,
    error::OlmResult,
    identities::{DeviceData, OtherUserIdentityData, OwnUserIdentityData, UserIdentityData},
    olm::{
        InboundGroupSession, PrivateCrossSigningIdentity, SenderDataFinder, SenderDataType,
        sender_data_finder::SessionDeviceCheckError,
    },
    store::{
        KeyQueryManager, Result as StoreResult, Store,
        caches::{SequenceNumber, StoreCache, StoreCacheGuard},
        types::{Changes, DeviceChanges, IdentityChanges, UserKeyQueryResult},
    },
    types::{
        CrossSigningKey, DeviceKeys, MasterPubkey, SelfSigningPubkey, UserSigningPubkey,
        requests::KeysQueryRequest,
    },
};

enum DeviceChange {
    New(DeviceData),
    Updated(DeviceData),
    None,
}

/// This enum helps us to distinguish between the changed and unchanged
/// identity case.
/// An unchanged identity means same cross signing keys as well as same
/// set of signatures on the master key.
enum IdentityUpdateResult {
    Updated(UserIdentityData),
    Unchanged(UserIdentityData),
}

#[derive(Debug, Clone)]
pub(crate) struct IdentityManager {
    /// Servers that have previously appeared in the `failures` section of a
    /// `/keys/query` response.
    ///
    /// See also [`crate::session_manager::SessionManager::failures`].
    failures: FailuresCache<OwnedServerName>,
    store: Store,

    pub(crate) key_query_manager: Arc<KeyQueryManager>,

    /// Details of the current "in-flight" key query request, if any
    keys_query_request_details: Arc<Mutex<Option<KeysQueryRequestDetails>>>,
}

/// Details of an in-flight key query request
#[derive(Debug, Clone, Default)]
struct KeysQueryRequestDetails {
    /// The sequence number, to be passed to
    /// `Store.mark_tracked_users_as_up_to_date`.
    sequence_number: SequenceNumber,

    /// A single batch of queries returned by the Store is broken up into one or
    /// more actual KeysQueryRequests, each with their own request id. We
    /// record the outstanding request ids here.
    request_ids: HashSet<OwnedTransactionId>,
}

// Helper type to handle key query response
struct KeySetInfo {
    user_id: OwnedUserId,
    master_key: MasterPubkey,
    self_signing: SelfSigningPubkey,
}

impl IdentityManager {
    const MAX_KEY_QUERY_USERS: usize = 250;

    pub fn new(store: Store) -> Self {
        let keys_query_request_details = Mutex::new(None);

        IdentityManager {
            store,
            key_query_manager: Default::default(),
            failures: Default::default(),
            keys_query_request_details: keys_query_request_details.into(),
        }
    }

    fn user_id(&self) -> &UserId {
        &self.store.static_account().user_id
    }

    /// Receive a successful `/keys/query` response.
    ///
    /// Returns a list of devices newly discovered devices and devices that
    /// changed.
    ///
    /// # Arguments
    ///
    /// * `request_id` - The request_id returned by `users_for_key_query` or
    ///   `build_key_query_for_users`
    /// * `response` - The response of the `/keys/query` request that the client
    ///   performed.
    pub async fn receive_keys_query_response(
        &self,
        request_id: &TransactionId,
        response: &KeysQueryResponse,
    ) -> OlmResult<(DeviceChanges, IdentityChanges)> {
        debug!(
            ?request_id,
            users = ?response.device_keys.keys().collect::<BTreeSet<_>>(),
            failures = ?response.failures,
            "Handling a `/keys/query` response"
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
        let (identities, cross_signing_identity) = self.handle_cross_signing_keys(response).await?;

        let changes = Changes {
            identities: identities.clone(),
            devices: devices.clone(),
            private_identity: cross_signing_identity,
            ..Default::default()
        };

        self.store.save_changes(changes).await?;

        // Update the sender data on any existing inbound group sessions based on the
        // changes in this response.
        //
        // `update_sender_data_from_device_changes` relies on being able to look up the
        // user identities from the store, so this has to happen *after* the
        // changes from `handle_cross_signing_keys` are saved.
        //
        // Note: it might be possible for this to race against session creation. If a
        // new session is received at the same time as a `/keys/query` response is being
        // processed, it could be saved without up-to-date sender data, but it might be
        // saved too late for it to be picked up by
        // `update_sender_data_from_device_changes`. However, this should be rare,
        // since, in general, /sync responses which might create a new session
        // are not processed at the same time as /keys/query responses (assuming
        // that the application does not call `OlmMachine::receive_sync_changes`
        // at the same time as `OlmMachine::mark_request_as_sent`).
        self.update_sender_data_from_device_changes(&devices).await?;

        // if this request is one of those we expected to be in flight, pass the
        // sequence number back to the store so that it can mark devices up to
        // date
        let sequence_number = {
            let mut request_details = self.keys_query_request_details.lock().await;

            request_details.as_mut().and_then(|details| {
                if details.request_ids.remove(request_id) {
                    Some(details.sequence_number)
                } else {
                    None
                }
            })
        };

        if let Some(sequence_number) = sequence_number {
            let cache = self.store.cache().await?;
            self.key_query_manager
                .synced(&cache)
                .await?
                .mark_tracked_users_as_up_to_date(
                    response.device_keys.keys().map(Deref::deref),
                    sequence_number,
                )
                .await?;
        }

        if enabled!(Level::DEBUG) {
            debug_log_keys_query_response(&devices, &identities, request_id);
        }

        Ok((devices, identities))
    }

    async fn update_or_create_device(
        store: Store,
        device_keys: DeviceKeys,
    ) -> StoreResult<DeviceChange> {
        let old_device =
            store.get_device_data(&device_keys.user_id, &device_keys.device_id).await?;

        if let Some(mut device) = old_device {
            match device.update_device(&device_keys) {
                Err(e) => {
                    warn!(
                        user_id = ?device.user_id(),
                        device_id = ?device.device_id(),
                        error = ?e,
                        "Rejecting device update",
                    );
                    Ok(DeviceChange::None)
                }
                Ok(true) => Ok(DeviceChange::Updated(device)),
                Ok(false) => Ok(DeviceChange::None),
            }
        } else {
            match DeviceData::try_from(&device_keys) {
                Ok(d) => {
                    // If this is our own device, check that the server isn't
                    // lying about our keys, also mark the device as locally
                    // trusted.
                    if d.user_id() == store.user_id() && d.device_id() == store.device_id() {
                        let local_device_keys = store.static_account().unsigned_device_keys();

                        if d.keys() == &local_device_keys.keys {
                            d.set_trust_state(LocalTrust::Verified);

                            trace!(
                                user_id = ?d.user_id(),
                                device_id = ?d.device_id(),
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
                            user_id = ?d.user_id(),
                            device_id = ?d.device_id(),
                            keys = ?d.keys(),
                            "Adding a new device to the device store",
                        );

                        Ok(DeviceChange::New(d))
                    }
                }
                Err(e) => {
                    warn!(
                        user_id = ?device_keys.user_id,
                        device_id = ?device_keys.device_id,
                        error = ?e,
                        "Rejecting a previously unseen device",
                    );

                    Ok(DeviceChange::None)
                }
            }
        }
    }

    async fn update_user_devices(
        store: Store,
        user_id: OwnedUserId,
        device_map: BTreeMap<OwnedDeviceId, Raw<ruma::encryption::DeviceKeys>>,
    ) -> StoreResult<DeviceChanges> {
        let own_device_id = store.static_account().device_id().to_owned();

        let mut changes = DeviceChanges::default();

        let current_devices: HashSet<OwnedDeviceId> = device_map.keys().cloned().collect();

        let tasks = device_map.into_iter().filter_map(|(device_id, device_keys)| match device_keys
            .deserialize_as::<DeviceKeys>(
        ) {
            Ok(device_keys) => {
                if user_id != device_keys.user_id || device_id != device_keys.device_id {
                    warn!(
                        ?user_id,
                        ?device_id,
                        device_key_user = ?device_keys.user_id,
                        device_key_device_id = ?device_keys.device_id,
                        "Mismatch in the device keys payload",
                    );
                    None
                } else {
                    Some(spawn(Self::update_or_create_device(store.clone(), device_keys)))
                }
            }
            Err(e) => {
                warn!(
                    ?user_id, ?device_id, error = ?e,
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
        let stored_devices = store.get_device_data_for_user(&user_id).await?;
        let stored_devices_set: HashSet<&OwnedDeviceId> = stored_devices.keys().collect();
        let deleted_devices_set = stored_devices_set.difference(&current_devices);

        let own_user_id = store.static_account().user_id();
        for device_id in deleted_devices_set {
            if user_id == *own_user_id && *device_id == &own_device_id {
                let identity_keys = store.static_account().identity_keys();

                warn!(
                    user_id = ?own_user_id,
                    device_id = ?own_device_id,
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
    ///   which the key query was done.
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
            spawn(Self::update_user_devices(self.store.clone(), user_id, device_keys_map))
        });

        let results = join_all(tasks).await;

        for result in results {
            let change_fragment = result.expect("Panic while updating user devices")?;

            changes.extend(change_fragment);
        }

        Ok(changes)
    }

    /// Check if the given public identity matches our stored private one.
    ///
    /// If they don't match, this is an indication that our identity has been
    /// rotated. In this case we return `Some(cleared_private_identity)`,
    /// where `cleared_private_identity` is our currently-stored
    /// private identity with the conflicting keys removed.
    ///
    /// Otherwise, assuming we do have a private master cross-signing key, we
    /// mark the public identity as verified.
    ///
    /// # Returns
    ///
    /// If the private identity needs updating (because it does not match the
    /// public keys), the updated private identity (which will need to be
    /// persisted).
    ///
    /// Otherwise, `None`.
    async fn check_private_identity(
        &self,
        identity: &OwnUserIdentityData,
    ) -> Option<PrivateCrossSigningIdentity> {
        let private_identity = self.store.private_identity();
        let private_identity = private_identity.lock().await;
        let result = private_identity.clear_if_differs(identity).await;

        if result.any_differ() {
            info!(cleared = ?result, "Removed some or all of our private cross signing keys");
            Some((*private_identity).clone())
        } else {
            // If the master key didn't rotate above (`clear_if_differs`),
            // then this means that the public part and the private parts of
            // the master key match. We previously did a signature check, so
            // this means that the private part of the master key has signed
            // the identity. We can safely mark the public part of the
            // identity as verified.
            if private_identity.has_master_key().await && !identity.is_verified() {
                trace!("Marked our own identity as verified");
                identity.mark_as_verified()
            }

            None
        }
    }

    /// Process an identity received in a `/keys/query` response that we
    /// previously knew about.
    ///
    /// If the identity is our own, we will look for a user-signing key; if one
    /// is not found, an error is returned. Otherwise, we then compare the
    /// received public identity against our stored private identity;
    /// if they match, the returned public identity is marked as verified and
    /// `*changed_private_identity` is set to `None`. If they do *not* match,
    /// it is an indication that our identity has been rotated, and
    /// `*changed_private_identity` is set to our currently-stored private
    /// identity with the conflicting keys removed (which will need to be
    /// persisted).
    ///
    /// Whether the identity is our own or that of another, we check whether
    /// there has been any change to the cross-signing keys, and classify
    /// the result into [`IdentityUpdateResult::Updated`] or
    /// [`IdentityUpdateResult::Unchanged`].
    ///
    /// # Arguments
    ///
    /// * `response` - The entire `/keys/query` response.
    /// * `master_key` - The public master cross-signing key from the
    ///   `/keys/query` response.
    /// * `self_signing` - The public self-signing key from the `/keys/query`
    ///   response.
    /// * `i` - The existing identity for this user.
    /// * `changed_private_identity` - Output parameter. Unchanged if the
    ///   identity is that of another user. If it is our own, set to `None` or
    ///   `Some` depending on whether our stored private identity needs
    ///   updating. See above for more detail.
    async fn handle_changed_identity(
        &self,
        response: &KeysQueryResponse,
        maybe_verified_own_identity: Option<&OwnUserIdentity>,
        master_key: MasterPubkey,
        self_signing: SelfSigningPubkey,
        i: UserIdentityData,
        changed_private_identity: &mut Option<PrivateCrossSigningIdentity>,
    ) -> Result<IdentityUpdateResult, SignatureError> {
        match i {
            UserIdentityData::Own(mut identity) => {
                let user_signing = self.get_user_signing_key_from_response(response)?;
                let has_changed = identity.update(master_key, self_signing, user_signing)?;
                *changed_private_identity = self.check_private_identity(&identity).await;
                if has_changed {
                    Ok(IdentityUpdateResult::Updated(identity.into()))
                } else {
                    Ok(IdentityUpdateResult::Unchanged(identity.into()))
                }
            }
            UserIdentityData::Other(mut identity) => {
                let has_changed = identity.update(
                    master_key,
                    self_signing,
                    maybe_verified_own_identity.map(|o| o.user_signing_key()),
                )?;

                if has_changed {
                    Ok(IdentityUpdateResult::Updated(identity.into()))
                } else {
                    Ok(IdentityUpdateResult::Unchanged(identity.into()))
                }
            }
        }
    }

    /// Process an identity received in a `/keys/query` response that we didn't
    /// previously know about.
    ///
    /// If the identity is our own, we will look for a user-signing key, and if
    /// it is present and correct, all three keys will be returned in the
    /// `IdentityChange` result; otherwise, an error is returned. We will also
    /// compare the received public identity against our stored private
    /// identity; if they match, the returned public identity is marked as
    /// verified and `*changed_private_identity` is set to `None`. If they do
    /// *not* match, it is an indication that our identity has been rotated,
    /// and `*changed_private_identity` is set to our currently-stored
    /// private identity with the conflicting keys removed (which will need
    /// to be persisted).
    ///
    /// If the identity is that of another user, we just parse the keys into the
    /// `IdentityChange` result, since all other checks have already been done.
    ///
    /// # Arguments
    ///
    /// * `response` - The entire `/keys/query` response.
    /// * `master_key` - The public master cross-signing key from the
    ///   `/keys/query` response.
    /// * `self_signing` - The public self-signing key from the `/keys/query`
    ///   response.
    /// * `changed_private_identity` - Output parameter. Unchanged if the
    ///   identity is that of another user. If it is our own, set to `None` or
    ///   `Some` depending on whether our stored private identity needs
    ///   updating. See above for more detail.
    async fn handle_new_identity(
        &self,
        response: &KeysQueryResponse,
        maybe_verified_own_identity: Option<&OwnUserIdentity>,
        master_key: MasterPubkey,
        self_signing: SelfSigningPubkey,
        changed_private_identity: &mut Option<PrivateCrossSigningIdentity>,
    ) -> Result<UserIdentityData, SignatureError> {
        if master_key.user_id() == self.user_id() {
            // Own identity
            let user_signing = self.get_user_signing_key_from_response(response)?;
            let identity = OwnUserIdentityData::new(master_key, self_signing, user_signing)?;
            *changed_private_identity = self.check_private_identity(&identity).await;
            Ok(identity.into())
        } else {
            // First time seen, create the identity. The current MSK will be pinned.
            let identity = OtherUserIdentityData::new(master_key, self_signing)?;
            let is_verified = maybe_verified_own_identity
                .is_some_and(|own_user_identity| own_user_identity.is_identity_signed(&identity));
            if is_verified {
                identity.mark_as_previously_verified();
            }

            Ok(identity.into())
        }
    }

    /// Try to deserialize the master key and self-signing key of an
    /// identity from a `/keys/query` response.
    ///
    /// Each user identity *must* at least contain a master and self-signing
    /// key, and this function deserializes them. (Our own identity, in addition
    /// to those two, also contains a user-signing key, but that is not
    /// extracted here; see
    /// [`IdentityManager::get_user_signing_key_from_response`])
    ///
    /// # Arguments
    ///
    ///  * `master_key` - The master key for a particular user from a
    ///    `/keys/query` response.
    ///  * `response` - The entire `/keys/query` response.
    ///
    /// # Returns
    ///
    /// `None` if the self-signing key couldn't be found in the response, or the
    /// one of the keys couldn't be deserialized. Else, the deserialized
    /// public keys.
    fn get_minimal_set_of_keys(
        master_key: &Raw<CrossSigningKey>,
        response: &KeysQueryResponse,
    ) -> Option<(MasterPubkey, SelfSigningPubkey)> {
        match master_key.deserialize_as_unchecked::<MasterPubkey>() {
            Ok(master_key) => {
                if let Some(self_signing) = response
                    .self_signing_keys
                    .get(master_key.user_id())
                    .and_then(|k| k.deserialize_as_unchecked::<SelfSigningPubkey>().ok())
                {
                    Some((master_key, self_signing))
                } else {
                    warn!(
                        "A user identity didn't contain a self signing pubkey or the key was invalid"
                    );
                    None
                }
            }
            Err(e) => {
                warn!(
                    error = ?e,
                    "Couldn't update or create new user identity"
                );
                None
            }
        }
    }

    /// Try to deserialize the our user-signing key from a `/keys/query`
    /// response.
    ///
    /// If a `/keys/query` response includes our own cross-signing keys, then it
    /// should include our user-signing key. This method attempts to
    /// extract, deserialize, and check the key from the response.
    ///
    /// # Arguments
    ///
    /// * `response` - the entire `/keys/query` response.
    fn get_user_signing_key_from_response(
        &self,
        response: &KeysQueryResponse,
    ) -> Result<UserSigningPubkey, SignatureError> {
        let Some(user_signing) = response
            .user_signing_keys
            .get(self.user_id())
            .and_then(|k| k.deserialize_as_unchecked::<UserSigningPubkey>().ok())
        else {
            warn!(
                "User identity for our own user didn't contain a user signing pubkey or the key \
                    isn't valid",
            );
            return Err(SignatureError::MissingSigningKey);
        };

        if user_signing.user_id() != self.user_id() {
            warn!(
                expected = ?self.user_id(),
                got = ?user_signing.user_id(),
                "User ID mismatch in our user-signing key",
            );
            return Err(SignatureError::UserIdMismatch);
        }

        Ok(user_signing)
    }

    /// Process the cross-signing keys for a particular identity from a
    /// `/keys/query` response.
    ///
    /// Checks that the keys are consistent, verifies the updates, and produces
    /// a list of changes to be stored.
    ///
    /// # Arguments
    ///
    /// * `response` - The entire `/keys/query` response.
    /// * `changes` - The identity results so far, which we will add to.
    /// * `changed_identity` - Output parameter: Unchanged if the identity is
    ///   that of another user. If it is our own, set to `None` or `Some`
    ///   depending on whether our stored private identity needs updating.
    /// * `maybe_verified_own_identity` - Own verified identity if any to check
    ///   verification status of updated identity.
    /// * `key_set_info` - The identity info as returned by the `/keys/query`
    ///   response.
    #[instrument(skip_all, fields(user_id))]
    async fn update_or_create_identity(
        &self,
        response: &KeysQueryResponse,
        changes: &mut IdentityChanges,
        changed_private_identity: &mut Option<PrivateCrossSigningIdentity>,
        maybe_verified_own_identity: Option<&OwnUserIdentity>,
        key_set_info: KeySetInfo,
    ) -> StoreResult<()> {
        let KeySetInfo { user_id, master_key, self_signing } = key_set_info;
        if master_key.user_id() != user_id || self_signing.user_id() != user_id {
            warn!(?user_id, "User ID mismatch in one of the cross signing keys");
        } else if let Some(i) = self.store.get_user_identity(&user_id).await? {
            // an identity we knew about before, which is being updated
            match self
                .handle_changed_identity(
                    response,
                    maybe_verified_own_identity,
                    master_key,
                    self_signing,
                    i,
                    changed_private_identity,
                )
                .await
            {
                Ok(IdentityUpdateResult::Updated(identity)) => {
                    trace!(?identity, "Updated a user identity");
                    changes.changed.push(identity);
                }
                Ok(IdentityUpdateResult::Unchanged(identity)) => {
                    trace!(?identity, "Received an unchanged user identity");
                    changes.unchanged.push(identity);
                }
                Err(e) => {
                    warn!(error = ?e, "Couldn't update an existing user identity");
                }
            }
        } else {
            // an identity we did not know about before
            match self
                .handle_new_identity(
                    response,
                    maybe_verified_own_identity,
                    master_key,
                    self_signing,
                    changed_private_identity,
                )
                .await
            {
                Ok(identity) => {
                    trace!(?identity, "Created new user identity");
                    changes.new.push(identity);
                }
                Err(e) => {
                    warn!(error = ?e, "Couldn't create new user identity");
                }
            }
        }

        Ok(())
    }

    /// Handle the cross signing keys part of a key query response.
    ///
    /// # Arguments
    ///
    /// * `response` - The `/keys/query` response.
    ///
    /// # Returns
    ///
    /// The processed results, to be saved to the datastore, comprising:
    ///
    ///  * A list of public identities that were received, categorised as "new",
    ///    "changed" or "unchanged".
    ///
    ///  * If our own identity was updated and did not match our private
    ///    identity, an update to that private identity. Otherwise, `None`.
    async fn handle_cross_signing_keys(
        &self,
        response: &KeysQueryResponse,
    ) -> StoreResult<(IdentityChanges, Option<PrivateCrossSigningIdentity>)> {
        let mut changes = IdentityChanges::default();
        let mut changed_identity = None;

        // We want to check if the updated/new other identities are trusted by us or
        // not. This is based on the current verified state of the own identity.
        let maybe_own_verified_identity = self
            .store
            .get_identity(self.user_id())
            .await?
            .and_then(UserIdentity::own)
            .filter(|own| own.is_verified());

        for (user_id, master_key) in &response.master_keys {
            // Get the master and self-signing key for each identity; those are required for
            // every user identity type. If we don't have those we skip over.
            let Some((master_key, self_signing)) =
                Self::get_minimal_set_of_keys(master_key.cast_ref(), response)
            else {
                continue;
            };

            let key_set_info = KeySetInfo { user_id: user_id.clone(), master_key, self_signing };

            self.update_or_create_identity(
                response,
                &mut changes,
                &mut changed_identity,
                maybe_own_verified_identity.as_ref(),
                key_set_info,
            )
            .await?;
        }

        Ok((changes, changed_identity))
    }

    /// Generate an "out-of-band" key query request for the given set of users.
    ///
    /// Unlike the regular key query requests returned by `users_for_key_query`,
    /// there can be several of these in flight at once. This can be useful
    /// if we need results to be as up-to-date as possible.
    ///
    /// Once the request has been made, the response can be fed back into the
    /// IdentityManager and store by calling `receive_keys_query_response`.
    ///
    /// # Arguments
    ///
    /// * `users` - list of users whose keys should be queried
    ///
    /// # Returns
    ///
    /// A tuple containing the request ID for the request, and the request
    /// itself.
    pub(crate) fn build_key_query_for_users<'a>(
        &self,
        users: impl IntoIterator<Item = &'a UserId>,
    ) -> (OwnedTransactionId, KeysQueryRequest) {
        // Since this is an "out-of-band" request, we just make up a transaction ID and
        // do not store the details in `self.keys_query_request_details`.
        //
        // `receive_keys_query_response` will process the response as normal, except
        // that it will not mark the users as "up-to-date".

        // We assume that there aren't too many users here; if we find a usecase that
        // requires lots of users to be up-to-date we may need to rethink this.
        (TransactionId::new(), KeysQueryRequest::new(users.into_iter().map(|u| u.to_owned())))
    }

    /// Get a list of key query requests needed.
    ///
    /// # Returns
    ///
    /// A map of a request ID to the `/keys/query` request.
    ///
    /// The response of a successful key query requests needs to be passed to
    /// the [`OlmMachine`] with the [`receive_keys_query_response`].
    ///
    /// [`receive_keys_query_response`]: Self::receive_keys_query_response
    pub async fn users_for_key_query(
        &self,
    ) -> StoreResult<BTreeMap<OwnedTransactionId, KeysQueryRequest>> {
        // Forget about any previous key queries in flight.
        *self.keys_query_request_details.lock().await = None;

        // We always want to track our own user, but in case we aren't in an encrypted
        // room yet, we won't be tracking ourselves yet. This ensures we are always
        // tracking ourselves.
        //
        // The check for emptiness is done first for performance.
        let (users, sequence_number) = {
            let cache = self.store.cache().await?;
            let key_query_manager = self.key_query_manager.synced(&cache).await?;

            let (users, sequence_number) = key_query_manager.users_for_key_query().await;

            if users.is_empty() && !key_query_manager.tracked_users().contains(self.user_id()) {
                key_query_manager.mark_user_as_changed(self.user_id()).await?;
                key_query_manager.users_for_key_query().await
            } else {
                (users, sequence_number)
            }
        };

        if users.is_empty() {
            Ok(BTreeMap::new())
        } else {
            // Let's remove users that are part of the `FailuresCache`. The cache, which is
            // a TTL cache, remembers users for which a previous `/key/query` request has
            // failed. We don't retry a `/keys/query` for such users for a
            // certain amount of time.
            let users = users.into_iter().filter(|u| !self.failures.contains(u.server_name()));

            // We don't want to create a single `/keys/query` request with an infinite
            // amount of users. Some servers will likely bail out after a
            // certain amount of users and the responses will be large. In the
            // case of a transmission error, we'll have to retransmit the large
            // response.
            //
            // Convert the set of users into multiple /keys/query requests.
            let requests: BTreeMap<_, _> = users
                .chunks(Self::MAX_KEY_QUERY_USERS)
                .into_iter()
                .map(|user_chunk| {
                    let request_id = TransactionId::new();
                    let request = KeysQueryRequest::new(user_chunk);

                    debug!(?request_id, users = ?request.device_keys.keys(), "Created a /keys/query request");

                    (request_id, request)
                })
                .collect();

            // Collect the request IDs, these will be used later in the
            // `receive_keys_query_response()` method to figure out if the user can be
            // marked as up-to-date/non-dirty.
            let request_ids = requests.keys().cloned().collect();
            let request_details = KeysQueryRequestDetails { sequence_number, request_ids };

            *self.keys_query_request_details.lock().await = Some(request_details);

            Ok(requests)
        }
    }

    /// Receive the list of users that contained changed devices from the
    /// `/sync` response.
    ///
    /// This will queue up the given user for a key query.
    ///
    /// Note: The user already needs to be tracked for it to be queued up for a
    /// key query.
    pub async fn receive_device_changes(
        &self,
        cache: &StoreCache,
        users: impl Iterator<Item = &UserId>,
    ) -> StoreResult<()> {
        self.key_query_manager.synced(cache).await?.mark_tracked_users_as_changed(users).await
    }

    /// See the docs for [`OlmMachine::update_tracked_users()`].
    pub async fn update_tracked_users(
        &self,
        users: impl IntoIterator<Item = &UserId>,
    ) -> StoreResult<()> {
        let cache = self.store.cache().await?;
        self.key_query_manager.synced(&cache).await?.update_tracked_users(users.into_iter()).await
    }

    /// Retrieve a list of a user's current devices, so we can encrypt a message
    /// to them.
    ///
    /// If we have not yet seen any devices for the user, and their device list
    /// has been marked as outdated, then we wait for the `/keys/query` request
    /// to complete. This helps ensure that we attempt at least once to fetch a
    /// user's devices before encrypting to them.
    pub async fn get_user_devices_for_encryption(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> StoreResult<HashMap<OwnedUserId, HashMap<OwnedDeviceId, DeviceData>>> {
        // How long we wait for /keys/query to complete.
        const KEYS_QUERY_WAIT_TIME: Duration = Duration::from_secs(5);

        let mut devices_by_user = HashMap::new();
        let mut users_with_no_devices_on_failed_servers = Vec::new();
        let mut users_with_no_devices_on_unfailed_servers = Vec::new();

        for user_id in users {
            // First of all, check the store for this user.
            let devices = self.store.get_device_data_for_user_filtered(user_id).await?;

            // Now, look for users who have no devices at all.
            //
            // If a user has no devices at all, that implies we have never (successfully)
            // done a `/keys/query` for them; we wait for one to complete if it is
            // in flight. (Of course, the user might genuinely have no devices, but
            // that's fine, it just means we redundantly grab the cache guard and
            // check the pending-query flag.)
            if !devices.is_empty() {
                // This user has at least one known device.
                //
                // The device list may also be outdated in this case; but in this
                // situation, we are racing between sending a message and retrieving their
                // device list. That's an inherently racy situation and there is no real
                // benefit to waiting for the `/keys/query` request to complete. So we don't
                // bother.
                //
                // We just add their devices to the result and carry on.
                devices_by_user.insert(user_id.to_owned(), devices);
                continue;
            }

            // *However*, if the user's server is currently subject to a backoff due to
            // previous failures, then `users_for_key_query` won't attempt to query
            // for the user's devices, so there's no point waiting.
            //
            // XXX: this is racy. It's possible that:
            //  * `failures` included the user's server when `users_for_key_query` was
            //    called, so the user was not returned in the `KeyQueryRequest`, and:
            //  * The backoff has now expired.
            //
            // In that case, we'll end up waiting for the *next* `users_for_key_query` call,
            // which might not be for 30 seconds or so. (And by then, it might be `failed`
            // again.)
            if self.failures.contains(user_id.server_name()) {
                users_with_no_devices_on_failed_servers.push(user_id);
                continue;
            }

            users_with_no_devices_on_unfailed_servers.push(user_id);
        }

        if !users_with_no_devices_on_failed_servers.is_empty() {
            info!(
                ?users_with_no_devices_on_failed_servers,
                "Not waiting for `/keys/query` for users whose server has previously failed"
            );
        }

        if !users_with_no_devices_on_unfailed_servers.is_empty() {
            // For each user with no devices, fire off a task to wait for a `/keys/query`
            // result if one is pending.
            //
            // We don't actually update the `devices_by_user` map here since that could
            // require concurrent access to it. Instead each task returns a
            // `(OwnedUserId, HashMap)` pair (or rather, an `Option` of one) so that we can
            // add the results to the map.
            let results = join_all(
                users_with_no_devices_on_unfailed_servers
                    .into_iter()
                    .map(|user_id| self.get_updated_keys_for_user(KEYS_QUERY_WAIT_TIME, user_id)),
            )
            .await;

            // Once all the tasks have completed, process the results.
            let mut updated_users = Vec::new();
            for result in results {
                if let Some((user_id, updated_devices)) = result? {
                    devices_by_user.insert(user_id.to_owned(), updated_devices);
                    updated_users.push(user_id);
                }
            }

            if !updated_users.is_empty() {
                info!(
                    ?updated_users,
                    "Waited for `/keys/query` to complete for users who have no devices"
                );
            }
        }

        Ok(devices_by_user)
    }

    /// Helper for get_user_devices_for_encryption.
    ///
    /// Waits for any pending `/keys/query` for the given user. If one was
    /// pending, reloads the device list and returns `Some(user_id,
    /// device_list)`. If no request was pending, returns `None`.
    #[allow(clippy::type_complexity)]
    #[instrument(skip(self))]
    async fn get_updated_keys_for_user<'a>(
        &self,
        timeout_duration: Duration,
        user_id: &'a UserId,
    ) -> Result<Option<(&'a UserId, HashMap<OwnedDeviceId, DeviceData>)>, CryptoStoreError> {
        let cache = self.store.cache().await?;
        match self
            .key_query_manager
            .wait_if_user_key_query_pending(cache, timeout_duration, user_id)
            .await?
        {
            UserKeyQueryResult::WasPending => {
                Ok(Some((user_id, self.store.get_device_data_for_user_filtered(user_id).await?)))
            }
            _ => Ok(None),
        }
    }

    /// Given a list of changed devices, update any [`InboundGroupSession`]s
    /// which were sent from those devices and which do not have complete
    /// sender data.
    async fn update_sender_data_from_device_changes(
        &self,
        device_changes: &DeviceChanges,
    ) -> Result<(), CryptoStoreError> {
        for device in device_changes.new.iter().chain(device_changes.changed.iter()) {
            // 1. Look for InboundGroupSessions from the device whose sender_data is
            //    UnknownDevice. For such sessions, we now have the device, and can update
            //    the sender_data accordingly.
            //
            // In theory, we only need to do this for new devices. In practice, I'm a bit
            // worried about races leading us to getting stuck in the
            // UnknownDevice state, so we'll paper over that by doing this check
            // on device updates too.
            self.update_sender_data_for_sessions_for_device(device, SenderDataType::UnknownDevice)
                .await?;

            // 2. If, and only if, the device is now correctly cross-signed (ie,
            //    device.is_cross_signed_by_owner() is true, and we have the master
            //    cross-signing key for the owner), look for InboundGroupSessions from the
            //    device whose sender_data is DeviceInfo. We can also update the sender_data
            //    for these sessions.
            //
            // In theory, we can skip a couple of steps of the SenderDataFinder algorithm,
            // because we're doing the cross-signing check here. In practice,
            // it's *way* easier just to use the same logic.
            let device_owner_identity = self.store.get_user_identity(device.user_id()).await?;
            if device_owner_identity.is_some_and(|id| device.is_cross_signed_by_owner(&id)) {
                self.update_sender_data_for_sessions_for_device(device, SenderDataType::DeviceInfo)
                    .await?;
            }
        }

        Ok(())
    }

    /// Given a device, look for [`InboundGroupSession`]s whose sender data is
    /// in the given state, and update it.
    #[instrument(skip(self))]
    async fn update_sender_data_for_sessions_for_device(
        &self,
        device: &DeviceData,
        sender_data_type: SenderDataType,
    ) -> Result<(), CryptoStoreError> {
        const IGS_BATCH_SIZE: usize = 50;

        let Some(curve_key) = device.curve25519_key() else { return Ok(()) };

        let mut last_session_id: Option<String> = None;
        loop {
            let mut sessions = self
                .store
                .get_inbound_group_sessions_for_device_batch(
                    curve_key,
                    sender_data_type,
                    last_session_id,
                    IGS_BATCH_SIZE,
                )
                .await?;

            if sessions.is_empty() {
                // end of the session list
                return Ok(());
            }

            last_session_id = None;
            for session in &mut sessions {
                last_session_id = Some(session.session_id().to_owned());
                self.update_sender_data_for_session(session, device).await?;
            }
            self.store.save_inbound_group_sessions(&sessions).await?;
        }
    }

    /// Update the sender data on the given inbound group session, using the
    /// given device data.
    #[instrument(skip(self, device, session), fields(session_id = session.session_id()))]
    async fn update_sender_data_for_session(
        &self,
        session: &mut InboundGroupSession,
        device: &DeviceData,
    ) -> Result<(), CryptoStoreError> {
        match SenderDataFinder::find_using_device_data(&self.store, device.clone(), session).await {
            Ok(sender_data) => {
                debug!("Updating existing InboundGroupSession with new SenderData {sender_data:?}");
                session.sender_data = sender_data;
            }
            Err(SessionDeviceCheckError::CryptoStoreError(e)) => {
                return Err(e);
            }
            Err(SessionDeviceCheckError::MismatchedIdentityKeys(e)) => {
                warn!(
                    ?session,
                    ?device,
                    "cannot update existing InboundGroupSession due to ownership error: {e}",
                );
            }
        }

        Ok(())
    }

    /// Mark all tracked users as dirty.
    ///
    /// All users *whose device lists we are tracking* are flagged as needing a
    /// key query. Users whose devices we are not tracking are ignored.
    pub(crate) async fn mark_all_tracked_users_as_dirty(
        &self,
        store_cache: StoreCacheGuard,
    ) -> StoreResult<()> {
        let store_wrapper = store_cache.store_wrapper();
        let tracked_users = store_wrapper.load_tracked_users().await?;

        self.key_query_manager
            .synced(&store_cache)
            .await?
            .mark_tracked_users_as_changed(
                tracked_users.iter().map(|tracked_user| tracked_user.user_id.as_ref()),
            )
            .await?;

        Ok(())
    }
}

/// Log information about what changed after processing a /keys/query response.
/// Only does anything if the DEBUG log level is enabled.
fn debug_log_keys_query_response(
    devices: &DeviceChanges,
    identities: &IdentityChanges,
    request_id: &TransactionId,
) {
    #[allow(unknown_lints, clippy::unwrap_or_default)] // false positive
    let changed_devices = devices.changed.iter().fold(BTreeMap::new(), |mut acc, d| {
        acc.entry(d.user_id()).or_insert_with(BTreeSet::new).insert(d.device_id());
        acc
    });

    #[allow(unknown_lints, clippy::unwrap_or_default)] // false positive
    let new_devices = devices.new.iter().fold(BTreeMap::new(), |mut acc, d| {
        acc.entry(d.user_id()).or_insert_with(BTreeSet::new).insert(d.device_id());
        acc
    });

    #[allow(unknown_lints, clippy::unwrap_or_default)] // false positive
    let deleted_devices = devices.deleted.iter().fold(BTreeMap::new(), |mut acc, d| {
        acc.entry(d.user_id()).or_insert_with(BTreeSet::new).insert(d.device_id());
        acc
    });

    let new_identities = identities.new.iter().map(|i| i.user_id()).collect::<BTreeSet<_>>();
    let changed_identities =
        identities.changed.iter().map(|i| i.user_id()).collect::<BTreeSet<_>>();

    debug!(
        ?request_id,
        ?new_devices,
        ?changed_devices,
        ?deleted_devices,
        ?new_identities,
        ?changed_identities,
        "Finished handling of the `/keys/query` response"
    );
}

#[cfg(any(test, feature = "testing"))]
#[allow(dead_code)]
pub(crate) mod testing {
    use std::sync::Arc;

    use matrix_sdk_test::ruma_response_from_json;
    use ruma::{
        DeviceId, UserId, api::client::keys::get_keys::v3::Response as KeyQueryResponse, device_id,
        user_id,
    };
    use serde_json::json;
    use tokio::sync::Mutex;

    use crate::{
        identities::IdentityManager,
        olm::{Account, PrivateCrossSigningIdentity},
        store::{CryptoStoreWrapper, MemoryStore, Store, types::PendingChanges},
        types::{DeviceKeys, requests::UploadSigningKeysRequest},
        verification::VerificationMachine,
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

    pub(crate) async fn manager_test_helper(
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> IdentityManager {
        let identity = PrivateCrossSigningIdentity::new(user_id.into());
        let identity = Arc::new(Mutex::new(identity));
        let user_id = user_id.to_owned();
        let account = Account::with_device_id(&user_id, device_id);
        let static_account = account.static_data().clone();
        let store = Arc::new(CryptoStoreWrapper::new(&user_id, device_id, MemoryStore::new()));
        let verification =
            VerificationMachine::new(static_account.clone(), identity.clone(), store.clone());
        let store = Store::new(static_account, identity, store, verification);
        store.save_pending_changes(PendingChanges { account: Some(account) }).await.unwrap();
        IdentityManager::new(store)
    }

    pub fn other_key_query() -> KeyQueryResponse {
        let data = &json!({
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
        });
        ruma_response_from_json(data)
    }

    // An updated version of `other_key_query` featuring an additional signature on
    // the master key *Note*: The added signature is actually not valid, but a
    // valid signature  is not required for our test.
    pub fn other_key_query_cross_signed() -> KeyQueryResponse {
        let data = json!({
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
                        },
                        // This is the added signature from alice USK compared to `other_key_query`. Note that actual signature is not valid.
                        "@alice:localhost": {
                            "ed25519:DU9z4gBFKFKCk7a13sW9wjT0Iyg7Hqv5f0BPM7DEhPo": "NotAValidSignature+GNtufuQ8lOf25byYLWIhmUYpPENdmM8nsldexD7vj+Sxoo7PknnTX/BL9h2N7uBq0JuykjunCAw"
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
        });
        ruma_response_from_json(&data)
    }

    /// Mocked response to a /keys/query request.
    pub fn own_key_query_with_user_id(user_id: &UserId) -> KeyQueryResponse {
        let data = json!({
          "device_keys": {
            user_id: {
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
                  user_id: {
                    "ed25519:WSKKLTJZCL": "SKpIUnq7QK0xleav0PrIQyKjVm+TgZr7Yi8cKjLeZDtkgyToE2d4/e3Aj79dqOlLB92jFVE4d1cM/Ry04wFwCA",
                    "ed25519:0C8lCBxrvrv/O7BQfsKnkYogHZX3zAgw3RfJuyiq210": "9UGu1iC5YhFCdELGfB29YaV+QE0t/X5UDSsPf4QcdZyXIwyp9zBbHX2lh9vWudNQ+akZpaq7ZRaaM+4TCnw/Ag"
                  }
                },
                "user_id": user_id,
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
                  user_id: {
                    "ed25519:LVWOVGOXME": "39Ir5Bttpc5+bQwzLj7rkjm5E5/cp/JTbMJ/t0enj6J5w9MXVBFOUqqM2hpaRaRwILMMpwYbJ8IOGjl0Y/MGAw"
                  }
                },
                "user_id": user_id,
                "unsigned": {
                  "device_display_name": "Non-cross signing"
                }
              }
            }
          },
          "failures": {},
          "master_keys": {
            user_id: {
              "user_id": user_id,
              "usage": [
                "master"
              ],
              "keys": {
                "ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0": "rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0"
              },
              "signatures": {
                user_id: {
                  "ed25519:WSKKLTJZCL": "ZzJp1wtmRdykXAUEItEjNiFlBrxx8L6/Vaen9am8AuGwlxxJtOkuY4m+4MPLvDPOgavKHLsrRuNLAfCeakMlCQ"
                }
              }
            }
          },
          "self_signing_keys": {
            user_id: {
              "user_id": user_id,
              "usage": [
                "self_signing"
              ],
              "keys": {
                "ed25519:0C8lCBxrvrv/O7BQfsKnkYogHZX3zAgw3RfJuyiq210": "0C8lCBxrvrv/O7BQfsKnkYogHZX3zAgw3RfJuyiq210"
              },
              "signatures": {
                user_id: {
                  "ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0": "AC7oDUW4rUhtInwb4lAoBJ0wAuu4a5k+8e34B5+NKsDB8HXRwgVwUWN/MRWc/sJgtSbVlhzqS9THEmQQ1C51Bw"
                }
              }
            }
          },
          "user_signing_keys": {
            user_id: {
              "user_id": user_id,
              "usage": [
                "user_signing"
              ],
              "keys": {
                "ed25519:DU9z4gBFKFKCk7a13sW9wjT0Iyg7Hqv5f0BPM7DEhPo": "DU9z4gBFKFKCk7a13sW9wjT0Iyg7Hqv5f0BPM7DEhPo"
              },
              "signatures": {
                user_id: {
                  "ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0": "C4L2sx9frGqj8w41KyynHGqwUbbwBYRZpYCB+6QWnvQFA5Oi/1PJj8w5anwzEsoO0TWmLYmf7FXuAGewanOWDg"
                }
              }
            }
          }
        });
        ruma_response_from_json(&data)
    }

    pub fn own_key_query() -> KeyQueryResponse {
        own_key_query_with_user_id(user_id())
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

        ruma_response_from_json(&json)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::ops::Deref;

    use futures_util::pin_mut;
    use matrix_sdk_test::{async_test, ruma_response_from_json, test_json};
    use ruma::{
        TransactionId, api::client::keys::get_keys::v3::Response as KeysQueryResponse, device_id,
        user_id,
    };
    use serde_json::json;
    use stream_assert::{assert_closed, assert_pending, assert_ready};

    use super::testing::{
        device_id, key_query, manager_test_helper, other_key_query, other_user_id, user_id,
    };
    use crate::{
        CrossSigningKeyExport, OlmMachine,
        identities::manager::testing::{other_key_query_cross_signed, own_key_query},
        olm::PrivateCrossSigningIdentity,
        store::types::Changes,
    };

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

        ruma_response_from_json(&response)
    }

    #[async_test]
    async fn test_tracked_users() {
        let manager = manager_test_helper(user_id(), device_id()).await;
        let alice = user_id!("@alice:example.org");

        let cache = manager.store.cache().await.unwrap();
        let key_query_manager = manager.key_query_manager.synced(&cache).await.unwrap();

        assert!(key_query_manager.tracked_users().is_empty(), "No users are initially tracked");

        manager.receive_device_changes(&cache, [alice].iter().map(Deref::deref)).await.unwrap();

        assert!(
            !key_query_manager.tracked_users().contains(alice),
            "Receiving a device changes update for a user we don't track does nothing"
        );

        assert!(
            !key_query_manager.users_for_key_query().await.0.contains(alice),
            "The user we don't track doesn't end up in the `/keys/query` request"
        );
    }

    #[async_test]
    async fn test_manager_creation() {
        let manager = manager_test_helper(user_id(), device_id()).await;
        let cache = manager.store.cache().await.unwrap();
        assert!(manager.key_query_manager.synced(&cache).await.unwrap().tracked_users().is_empty())
    }

    #[async_test]
    async fn test_manager_key_query_response() {
        let manager = manager_test_helper(user_id(), device_id()).await;
        let other_user = other_user_id();
        let devices = manager.store.get_user_devices(other_user).await.unwrap();
        assert_eq!(devices.devices().count(), 0);

        manager
            .receive_keys_query_response(&TransactionId::new(), &other_key_query())
            .await
            .unwrap();

        let devices = manager.store.get_user_devices(other_user).await.unwrap();
        assert_eq!(devices.devices().count(), 1);

        let device = manager
            .store
            .get_device_data(other_user, device_id!("SKISMLNIMH"))
            .await
            .unwrap()
            .unwrap();
        let identity = manager.store.get_user_identity(other_user).await.unwrap().unwrap();
        let identity = identity.other().unwrap();

        assert!(identity.is_device_signed(&device));
    }

    #[async_test]
    async fn test_manager_own_key_query_response() {
        let manager = manager_test_helper(user_id(), device_id()).await;
        let our_user = user_id();
        let devices = manager.store.get_user_devices(our_user).await.unwrap();
        assert_eq!(devices.devices().count(), 0);

        let private_identity = manager.store.private_identity();
        let private_identity = private_identity.lock().await;
        let identity_request = private_identity.as_upload_request().await;
        drop(private_identity);

        let device_keys =
            manager.store.cache().await.unwrap().account().await.unwrap().device_keys();
        manager
            .receive_keys_query_response(
                &TransactionId::new(),
                &key_query(identity_request, device_keys),
            )
            .await
            .unwrap();

        let identity = manager
            .store
            .get_user_identity(our_user)
            .await
            .unwrap()
            .expect("missing user identity");
        let identity = identity.own().expect("missing own identity");
        assert!(identity.is_verified());

        let devices = manager.store.get_user_devices(our_user).await.unwrap();
        assert_eq!(devices.devices().count(), 1);

        let device =
            manager.store.get_device_data(our_user, device_id!(device_id())).await.unwrap();

        assert!(device.is_some());
    }

    #[async_test]
    async fn test_private_identity_invalidation_after_public_keys_change() {
        let user_id = user_id!("@example1:localhost");
        let manager = manager_test_helper(user_id, "DEVICEID".into()).await;

        let identity_request = {
            let private_identity = manager.store.private_identity();
            let private_identity = private_identity.lock().await;
            private_identity.as_upload_request().await
        };
        let device_keys = manager.store.static_account().unsigned_device_keys();

        let response = json!({
            "device_keys": {
                user_id: {
                    device_keys.device_id.to_string(): device_keys
                }
            },
            "master_keys": {
                user_id: identity_request.master_key,
            },
            "self_signing_keys": {
                user_id: identity_request.self_signing_key,
            },
            "user_signing_keys": {
                user_id: identity_request.user_signing_key,
            }
        });

        let response = ruma_response_from_json(&response);
        manager.receive_keys_query_response(&TransactionId::new(), &response).await.unwrap();

        let identity = manager.store.get_user_identity(user_id).await.unwrap().unwrap();
        let identity = identity.own().unwrap();
        assert!(identity.is_verified());

        let identity_request = {
            let private_identity = PrivateCrossSigningIdentity::new(user_id.into());
            private_identity.as_upload_request().await
        };

        let response = json!({
            "master_keys": {
                user_id: identity_request.master_key,
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
                },
            },
            "self_signing_keys": {
                user_id: identity_request.self_signing_key,
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
            "user_signing_keys": {
                user_id: identity_request.user_signing_key,
            }
        });

        let response = ruma_response_from_json(&response);
        let (_, private_identity) = manager.handle_cross_signing_keys(&response).await.unwrap();

        assert!(private_identity.is_some());
        let private_identity = manager.store.private_identity();
        assert!(private_identity.lock().await.is_empty().await);
    }

    #[async_test]
    async fn test_no_tracked_users_key_query_request() {
        let manager = manager_test_helper(user_id(), device_id()).await;

        let cache = manager.store.cache().await.unwrap();
        assert!(
            manager.key_query_manager.synced(&cache).await.unwrap().tracked_users().is_empty(),
            "No users are initially tracked"
        );

        let requests = manager.users_for_key_query().await.unwrap();
        assert!(!requests.is_empty(), "We query the keys for our own user");

        assert!(
            manager
                .key_query_manager
                .synced(&cache)
                .await
                .unwrap()
                .tracked_users()
                .contains(manager.user_id()),
            "Our own user is now tracked"
        );
    }

    /// If a user is invalidated while a /keys/query request is in flight, that
    /// user is not removed from the list of outdated users when the
    /// response is received
    #[async_test]
    async fn test_invalidation_race_handling() {
        let manager = manager_test_helper(user_id(), device_id()).await;
        let alice = other_user_id();
        manager.update_tracked_users([alice]).await.unwrap();

        // alice should be in the list of key queries
        let (reqid, req) = manager.users_for_key_query().await.unwrap().pop_first().unwrap();
        assert!(req.device_keys.contains_key(alice));

        // another invalidation turns up
        {
            let cache = manager.store.cache().await.unwrap();
            manager.receive_device_changes(&cache, [alice].into_iter()).await.unwrap();
        }

        // the response from the query arrives
        manager.receive_keys_query_response(&reqid, &other_key_query()).await.unwrap();

        // alice should *still* be in the list of key queries
        let (reqid, req) = manager.users_for_key_query().await.unwrap().pop_first().unwrap();
        assert!(req.device_keys.contains_key(alice));

        // another key query response
        manager.receive_keys_query_response(&reqid, &other_key_query()).await.unwrap();

        // finally alice should not be in the list
        let queries = manager.users_for_key_query().await.unwrap();
        assert!(!queries.iter().any(|(_, r)| r.device_keys.contains_key(alice)));
    }

    #[async_test]
    async fn test_failure_handling() {
        let manager = manager_test_helper(user_id(), device_id()).await;
        let alice = user_id!("@alice:example.org");

        {
            let cache = manager.store.cache().await.unwrap();
            let key_query_manager = manager.key_query_manager.synced(&cache).await.unwrap();
            assert!(key_query_manager.tracked_users().is_empty(), "No users are initially tracked");

            key_query_manager.mark_user_as_changed(alice).await.unwrap();

            assert!(
                key_query_manager.tracked_users().contains(alice),
                "Alice is tracked after being marked as tracked"
            );
        }

        let (reqid, req) = manager.users_for_key_query().await.unwrap().pop_first().unwrap();
        assert!(req.device_keys.contains_key(alice));

        // a failure should stop us querying for the user's keys.
        let response = key_query_with_failures();
        manager.receive_keys_query_response(&reqid, &response).await.unwrap();
        assert!(manager.failures.contains(alice.server_name()));
        assert!(
            !manager
                .users_for_key_query()
                .await
                .unwrap()
                .iter()
                .any(|(_, r)| r.device_keys.contains_key(alice))
        );

        // clearing the failure flag should make the user reappear in the query list.
        manager.failures.remove([alice.server_name().to_owned()].iter());
        assert!(
            manager
                .users_for_key_query()
                .await
                .unwrap()
                .iter()
                .any(|(_, r)| r.device_keys.contains_key(alice))
        );
    }

    #[async_test]
    async fn test_out_of_band_key_query() {
        // build the request
        let manager = manager_test_helper(user_id(), device_id()).await;
        let (reqid, req) = manager.build_key_query_for_users(vec![user_id()]);
        assert!(req.device_keys.contains_key(user_id()));

        // make up a response and check it is processed
        let (device_changes, identity_changes) =
            manager.receive_keys_query_response(&reqid, &own_key_query()).await.unwrap();
        assert_eq!(device_changes.new.len(), 1);
        assert_eq!(device_changes.new[0].device_id(), "LVWOVGOXME");
        assert_eq!(identity_changes.new.len(), 1);
        assert_eq!(identity_changes.new[0].user_id(), user_id());

        let devices = manager.store.get_user_devices(user_id()).await.unwrap();
        assert_eq!(devices.devices().count(), 1);
        assert_eq!(devices.devices().next().unwrap().device_id(), "LVWOVGOXME");
    }

    #[async_test]
    async fn test_invalid_key_response() {
        let my_user_id = user_id();
        let my_device_id = device_id();
        let manager = manager_test_helper(my_user_id, my_device_id).await;

        // First of all, populate the store with good data
        let (reqid, _) = manager.build_key_query_for_users(vec![user_id()]);
        let (device_changes, identity_changes) =
            manager.receive_keys_query_response(&reqid, &own_key_query()).await.unwrap();
        assert_eq!(device_changes.new.len(), 1);
        let test_device_id = device_changes.new.first().unwrap().device_id().to_owned();
        let changes =
            Changes { devices: device_changes, identities: identity_changes, ..Changes::default() };
        manager.store.save_changes(changes).await.unwrap();

        // Now provide an invalid update
        let (reqid, _) = manager.build_key_query_for_users(vec![my_user_id]);
        let response = ruma_response_from_json(&json!({
            "device_keys": {
                my_user_id: {
                    test_device_id.as_str(): {
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                        ],
                        "device_id": test_device_id.as_str(),
                        "keys": {
                            format!("curve25519:{}", test_device_id): "wnip2tbJBJxrFayC88NNJpm61TeSNgYcqBH4T9yEDhU",
                            format!("ed25519:{}", test_device_id): "lQ+eshkhgKoo+qp9Qgnj3OX5PBoWMU5M9zbuEevwYqE"
                        },
                        "signatures": {
                            my_user_id: {
                                // Not a valid signature.
                                format!("ed25519:{}", test_device_id): "imadethisup",
                            }
                        },
                        "user_id": my_user_id,
                    }
                }
            }
        }));

        let (device_changes, identity_changes) =
            manager.receive_keys_query_response(&reqid, &response).await.unwrap();

        // The result should be empty
        assert_eq!(device_changes.new.len(), 0);
        assert_eq!(device_changes.changed.len(), 0);
        assert_eq!(device_changes.deleted.len(), 0);
        assert_eq!(identity_changes.new.len(), 0);

        // And the device should not have been updated.
        let device =
            manager.store.get_user_devices(my_user_id).await.unwrap().get(&test_device_id).unwrap();
        assert_eq!(device.algorithms().len(), 2);
    }

    #[async_test]
    async fn test_devices_stream() {
        let manager = manager_test_helper(user_id(), device_id()).await;
        let (request_id, _) = manager.build_key_query_for_users(vec![user_id()]);

        let stream = manager.store.devices_stream();
        pin_mut!(stream);

        manager.receive_keys_query_response(&request_id, &own_key_query()).await.unwrap();

        let update = assert_ready!(stream);
        assert!(!update.new.is_empty(), "The device update should contain some devices");
    }

    #[async_test]
    async fn test_identities_stream() {
        let manager = manager_test_helper(user_id(), device_id()).await;
        let (request_id, _) = manager.build_key_query_for_users(vec![user_id()]);

        let stream = manager.store.user_identities_stream();
        pin_mut!(stream);

        manager.receive_keys_query_response(&request_id, &own_key_query()).await.unwrap();

        let update = assert_ready!(stream);
        assert!(!update.new.is_empty(), "The identities update should contain some identities");
    }

    #[async_test]
    async fn test_identities_stream_raw() {
        let mut manager = Some(manager_test_helper(user_id(), device_id()).await);
        let (request_id, _) = manager.as_ref().unwrap().build_key_query_for_users(vec![user_id()]);

        let stream = manager.as_ref().unwrap().store.identities_stream_raw();
        pin_mut!(stream);

        manager
            .as_ref()
            .unwrap()
            .receive_keys_query_response(&request_id, &own_key_query())
            .await
            .unwrap();

        let (identity_update, _) = assert_ready!(stream);
        assert_eq!(identity_update.new.len(), 1);
        assert_eq!(identity_update.changed.len(), 0);
        assert_eq!(identity_update.unchanged.len(), 0);
        assert_eq!(identity_update.new[0].user_id(), user_id());

        assert_pending!(stream);

        let (new_request_id, _) =
            manager.as_ref().unwrap().build_key_query_for_users(vec![user_id()]);

        // A second `/keys/query` response with the same result shouldn't fire a change
        // notification: the identity and device should be unchanged.
        manager
            .as_ref()
            .unwrap()
            .receive_keys_query_response(&new_request_id, &own_key_query())
            .await
            .unwrap();

        assert_pending!(stream);

        // dropping the manager (and hence dropping the store) should close the stream
        manager.take();
        assert_closed!(stream);
    }

    #[async_test]
    async fn test_identities_stream_raw_signature_update() {
        let mut manager = Some(manager_test_helper(user_id(), device_id()).await);
        let (request_id, _) =
            manager.as_ref().unwrap().build_key_query_for_users(vec![other_user_id()]);

        let stream = manager.as_ref().unwrap().store.identities_stream_raw();
        pin_mut!(stream);

        manager
            .as_ref()
            .unwrap()
            .receive_keys_query_response(&request_id, &other_key_query())
            .await
            .unwrap();

        let (identity_update, _) = assert_ready!(stream);
        assert_eq!(identity_update.new.len(), 1);
        assert_eq!(identity_update.changed.len(), 0);
        assert_eq!(identity_update.unchanged.len(), 0);
        assert_eq!(identity_update.new[0].user_id(), other_user_id());

        let initial_msk = identity_update.new[0].master_key().clone();

        let (new_request_id, _) =
            manager.as_ref().unwrap().build_key_query_for_users(vec![user_id()]);
        // There is a new signature on the msk, should trigger a change
        manager
            .as_ref()
            .unwrap()
            .receive_keys_query_response(&new_request_id, &other_key_query_cross_signed())
            .await
            .unwrap();

        let (identity_update_2, _) = assert_ready!(stream);
        assert_eq!(identity_update_2.new.len(), 0);
        assert_eq!(identity_update_2.changed.len(), 1);
        assert_eq!(identity_update_2.unchanged.len(), 0);

        let updated_msk = identity_update_2.changed[0].master_key().clone();

        // Identity has a change (new signature) but it's the same msk
        assert_eq!(initial_msk, updated_msk);

        assert_pending!(stream);

        manager.take();
    }

    #[async_test]
    async fn test_key_query_with_unknown_properties() {
        let manager = manager_test_helper(user_id(), device_id()).await;
        let other_user = user_id!("@example:localhost");
        let devices = manager.store.get_user_devices(other_user).await.unwrap();
        assert_eq!(devices.devices().count(), 0);

        let response = json!({
            "device_keys": {
                "@example:localhost": {
                    "OBEBOSKTBE": {
                        "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                        "user_id": "@example:localhost",
                        "device_id": "OBEBOSKTBE",
                        "extra_property": "somevalue",
                        "keys": {
                            "curve25519:OBEBOSKTBE": "ECrdZebl0DskwbkxoztsiKPb6ivu7M2qQ70BFWwre3w",
                            "ed25519:OBEBOSKTBE": "hFWo+pG6TVWNzq/ZubUQVL5Ardu9rqHxpKkCbf1/KiA"
                        },
                        "signatures": {
                            "@example:localhost": {
                                "ed25519:OBEBOSKTBE": "6vyYUgX+IoT1x6Mvf0g/GEPVb2UI3brfL7WZ75WZ81sH4FBFgAzkkuGpw9suGLKXnlEdLH0suBzaT4esVhFDCw",
                            },
                        },
                    },
                },
            },
        });

        let response = ruma_response_from_json(&response);
        manager.receive_keys_query_response(&TransactionId::new(), &response).await.unwrap();

        let devices = manager.store.get_user_devices(other_user).await.unwrap();
        assert_eq!(devices.devices().count(), 1);

        manager.store.get_device_data(other_user, device_id!("OBEBOSKTBE")).await.unwrap().unwrap();
    }

    #[async_test]
    async fn test_manager_identity_updates() {
        use test_json::keys_query_sets::IdentityChangeDataSet as DataSet;

        let manager = manager_test_helper(user_id(), device_id()).await;
        let other_user = DataSet::user_id();
        let devices = manager.store.get_user_devices(other_user).await.unwrap();
        assert_eq!(devices.devices().count(), 0);

        let identity = manager.store.get_user_identity(other_user).await.unwrap();
        assert!(identity.is_none());

        manager
            .receive_keys_query_response(
                &TransactionId::new(),
                &DataSet::key_query_with_identity_a(),
            )
            .await
            .unwrap();

        let identity = manager.store.get_user_identity(other_user).await.unwrap().unwrap();
        let other_identity = identity.other().unwrap();

        // We should now have an identity for the user but no pin violation
        // (pinned master key is the current one)
        assert!(!other_identity.has_pin_violation());
        let first_device =
            manager.store.get_device_data(other_user, DataSet::device_a()).await.unwrap().unwrap();
        assert!(first_device.is_cross_signed_by_owner(&identity));

        // We receive a new keys update for that user, with a new identity
        manager
            .receive_keys_query_response(
                &TransactionId::new(),
                &DataSet::key_query_with_identity_b(),
            )
            .await
            .unwrap();

        let identity = manager.store.get_user_identity(other_user).await.unwrap().unwrap();
        let other_identity = identity.other().unwrap();

        // The previous known identity has been replaced, there should be a pin
        // violation
        assert!(other_identity.has_pin_violation());

        let second_device =
            manager.store.get_device_data(other_user, DataSet::device_b()).await.unwrap().unwrap();

        // There is a new device signed by the new identity
        assert!(second_device.is_cross_signed_by_owner(&identity));

        // The first device should not be signed by the new identity
        let first_device =
            manager.store.get_device_data(other_user, DataSet::device_a()).await.unwrap().unwrap();
        assert!(!first_device.is_cross_signed_by_owner(&identity));

        let remember_previous_identity = other_identity.clone();
        // We receive updated keys for that user, with no identity anymore.
        // Notice that there is no server API to delete identity, but we want to
        // test here that a home server cannot clear the identity and
        // subsequently serve a new one which would get automatically approved.
        manager
            .receive_keys_query_response(
                &TransactionId::new(),
                &DataSet::key_query_with_identity_no_identity(),
            )
            .await
            .unwrap();

        let identity = manager.store.get_user_identity(other_user).await.unwrap().unwrap();
        let other_identity = identity.other().unwrap();

        assert_eq!(other_identity, &remember_previous_identity);
        assert!(other_identity.has_pin_violation());
    }

    #[async_test]
    async fn test_manager_resolve_identity_pin_violation() {
        use test_json::keys_query_sets::IdentityChangeDataSet as DataSet;

        let manager = manager_test_helper(user_id(), device_id()).await;
        let other_user = DataSet::user_id();

        manager
            .receive_keys_query_response(
                &TransactionId::new(),
                &DataSet::key_query_with_identity_a(),
            )
            .await
            .unwrap();

        // We receive a new keys update for that user, with a new identity
        manager
            .receive_keys_query_response(
                &TransactionId::new(),
                &DataSet::key_query_with_identity_b(),
            )
            .await
            .unwrap();

        let identity = manager.store.get_user_identity(other_user).await.unwrap().unwrap();
        let other_identity = identity.other().unwrap();

        // We have a new identity now, so there should be a pin violation
        assert!(other_identity.has_pin_violation());

        // Resolve the violation by pinning the new identity
        other_identity.pin();

        assert!(!other_identity.has_pin_violation());
    }

    // Set up a machine do initial own key query and import cross-signing secret to
    // make the current session verified.
    async fn common_verified_identity_changes_machine_setup() -> OlmMachine {
        use test_json::keys_query_sets::VerificationViolationTestData as DataSet;

        let machine = OlmMachine::new(DataSet::own_id(), device_id!("LOCAL")).await;

        let keys_query = DataSet::own_keys_query_response_1();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        machine
            .import_cross_signing_keys(CrossSigningKeyExport {
                master_key: DataSet::MASTER_KEY_PRIVATE_EXPORT.to_owned().into(),
                self_signing_key: DataSet::SELF_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
                user_signing_key: DataSet::USER_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
            })
            .await
            .unwrap();
        machine
    }
    #[async_test]
    async fn test_manager_verified_latch_setup_on_new_identities() {
        use test_json::keys_query_sets::VerificationViolationTestData as DataSet;

        let machine = common_verified_identity_changes_machine_setup().await;

        // ######
        // First test: Assert that the latch is properly set on new identities
        // ######
        let keys_query = DataSet::bob_keys_query_response_signed();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let own_identity =
            machine.get_identity(DataSet::own_id(), None).await.unwrap().unwrap().own().unwrap();
        // For sanity check that own identity is trusted
        assert!(own_identity.is_verified());

        let bob_identity =
            machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap().other().unwrap();
        // The verified latch should be true
        assert!(bob_identity.was_previously_verified());
        // And bob is verified
        assert!(bob_identity.is_verified());

        // ######
        // Second test: Assert that the local latch stays on if the identity is rotated
        // ######
        let keys_query = DataSet::bob_keys_query_response_rotated();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let bob_identity =
            machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap().other().unwrap();
        // Bob is not verified anymore
        assert!(!bob_identity.is_verified());
        // The verified latch should still be true
        assert!(bob_identity.was_previously_verified());
        // Bob device_2 is self-signed even if there is this verification latch
        // violation
        let bob_device = machine
            .get_device(DataSet::bob_id(), DataSet::bob_device_2_id(), None)
            .await
            .unwrap()
            .unwrap();
        assert!(bob_identity.is_device_signed(&bob_device));
        // there is also a pin violation
        assert!(bob_identity.has_pin_violation());
        // Fixing the pin violation won't fix the verification latch violation
        bob_identity.pin_current_master_key().await.unwrap();
        assert!(!bob_identity.has_pin_violation());
        let has_latch_violation =
            bob_identity.was_previously_verified() && !bob_identity.is_verified();
        assert!(has_latch_violation);
    }

    #[async_test]
    async fn test_manager_verified_identity_changes_setup_on_updated_identities() {
        use test_json::keys_query_sets::VerificationViolationTestData as DataSet;

        let machine = common_verified_identity_changes_machine_setup().await;

        // ######
        // Get the Carol identity for the first time
        // ######
        let keys_query = DataSet::carol_keys_query_response_unsigned();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let carol_identity =
            machine.get_identity(DataSet::carol_id(), None).await.unwrap().unwrap();
        // The identity is not verified
        assert!(!carol_identity.is_verified());
        // The verified latch is off
        assert!(!carol_identity.was_previously_verified());

        // Carol is verified, likely from another session. Ensure the latch is updated
        // when the key query response is processed
        let keys_query = DataSet::carol_keys_query_response_signed();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let carol_identity = machine
            .get_identity(DataSet::carol_id(), None)
            .await
            .unwrap()
            .unwrap()
            .other()
            .unwrap();
        assert!(carol_identity.is_verified());
        // This should have updated the latch
        assert!(carol_identity.was_previously_verified());
        // It is the same identity, it's just signed now so no pin violation
        assert!(!carol_identity.has_pin_violation());
    }

    // Set up a machine do initial own key query.
    // The cross signing secrets are not yet uploaded.
    // Then query keys for carol and bob (both signed by own identity)
    async fn common_verified_identity_changes_own_trust_change_machine_setup() -> OlmMachine {
        use test_json::keys_query_sets::VerificationViolationTestData as DataSet;

        // Start on a non-verified session
        let machine = OlmMachine::new(DataSet::own_id(), device_id!("LOCAL")).await;

        let keys_query = DataSet::own_keys_query_response_1();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        // For sanity check that own identity is not trusted
        let own_identity =
            machine.get_identity(DataSet::own_id(), None).await.unwrap().unwrap().own().unwrap();
        assert!(!own_identity.is_verified());

        let keys_query = DataSet::own_keys_query_response_1();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        // Get Bob and Carol already signed
        let keys_query = DataSet::bob_keys_query_response_signed();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        let keys_query = DataSet::carol_keys_query_response_signed();
        let txn_id = TransactionId::new();
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

        machine.update_tracked_users(vec![DataSet::bob_id(), DataSet::carol_id()]).await.unwrap();

        machine
    }

    #[async_test]
    async fn test_manager_verified_identity_changes_setup_on_own_identity_trust_change() {
        use test_json::keys_query_sets::VerificationViolationTestData as DataSet;
        let machine = common_verified_identity_changes_own_trust_change_machine_setup().await;

        let own_identity =
            machine.get_identity(DataSet::own_id(), None).await.unwrap().unwrap().own().unwrap();

        let bob_identity = machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap();
        // Bob is verified by our identity but our own identity is not yet trusted
        assert!(!bob_identity.was_previously_verified());
        assert!(own_identity.is_identity_signed(&bob_identity.other().unwrap()));

        let carol_identity =
            machine.get_identity(DataSet::carol_id(), None).await.unwrap().unwrap();
        // Carol is verified by our identity but our own identity is not yet trusted
        assert!(!carol_identity.was_previously_verified());
        assert!(own_identity.is_identity_signed(&carol_identity.other().unwrap()));

        // Marking our own identity as trusted should update the existing identities
        let _ = own_identity.verify().await;

        let own_identity = machine.get_identity(DataSet::own_id(), None).await.unwrap().unwrap();
        assert!(own_identity.is_verified());

        let carol_identity =
            machine.get_identity(DataSet::carol_id(), None).await.unwrap().unwrap();
        assert!(carol_identity.is_verified());
        // The latch should be set now
        assert!(carol_identity.was_previously_verified());

        let bob_identity = machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap();
        assert!(bob_identity.is_verified());
        // The latch should be set now
        assert!(bob_identity.was_previously_verified());
    }

    #[async_test]
    async fn test_manager_verified_identity_change_setup_on_import_secrets() {
        use test_json::keys_query_sets::VerificationViolationTestData as DataSet;
        let machine = common_verified_identity_changes_own_trust_change_machine_setup().await;

        let own_identity =
            machine.get_identity(DataSet::own_id(), None).await.unwrap().unwrap().own().unwrap();

        let bob_identity =
            machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap().other().unwrap();
        // Carol is verified by our identity but our own identity is not yet trusted
        assert!(own_identity.is_identity_signed(&bob_identity));
        assert!(!bob_identity.was_previously_verified());

        let carol_identity = machine
            .get_identity(DataSet::carol_id(), None)
            .await
            .unwrap()
            .unwrap()
            .other()
            .unwrap();
        // Carol is verified by our identity but our own identity is not yet trusted
        assert!(own_identity.is_identity_signed(&carol_identity));
        assert!(!carol_identity.was_previously_verified());

        // Marking our own identity as trusted should update the existing identities
        machine
            .import_cross_signing_keys(CrossSigningKeyExport {
                master_key: DataSet::MASTER_KEY_PRIVATE_EXPORT.to_owned().into(),
                self_signing_key: DataSet::SELF_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
                user_signing_key: DataSet::USER_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
            })
            .await
            .unwrap();

        let own_identity = machine.get_identity(DataSet::own_id(), None).await.unwrap().unwrap();
        assert!(own_identity.is_verified());

        let carol_identity =
            machine.get_identity(DataSet::carol_id(), None).await.unwrap().unwrap();
        assert!(carol_identity.is_verified());
        // The latch should be set now
        assert!(carol_identity.was_previously_verified());

        let bob_identity = machine.get_identity(DataSet::bob_id(), None).await.unwrap().unwrap();
        assert!(bob_identity.is_verified());
        // The latch should be set now
        assert!(bob_identity.was_previously_verified());
    }

    mod update_sender_data {
        use assert_matches::assert_matches;
        use matrix_sdk_test::async_test;
        use ruma::room_id;

        use super::{device_id, manager_test_helper};
        use crate::{
            Account, DeviceData, EncryptionSettings,
            identities::manager::testing::{other_user_id, user_id},
            olm::{InboundGroupSession, SenderData},
            store::types::{Changes, DeviceChanges},
        };

        #[async_test]
        async fn test_adds_device_info_to_existing_sessions() {
            let manager = manager_test_helper(user_id(), device_id()).await;

            // Given that we have lots of sessions in the store, from each of two devices
            let account1 = Account::new(user_id());
            let account2 = Account::new(other_user_id());

            let mut account1_sessions = Vec::new();
            for _ in 0..60 {
                account1_sessions.push(create_inbound_group_session(&account1).await);
            }
            let mut account2_sessions = Vec::new();
            for _ in 0..60 {
                account2_sessions.push(create_inbound_group_session(&account2).await);
            }
            manager
                .store
                .save_changes(Changes {
                    inbound_group_sessions: [account1_sessions.clone(), account2_sessions.clone()]
                        .concat(),
                    ..Default::default()
                })
                .await
                .unwrap();

            // When we get an update for one device
            let device_data = DeviceData::from_account(&account1);
            manager
                .update_sender_data_from_device_changes(&DeviceChanges {
                    changed: vec![device_data],
                    ..Default::default()
                })
                .await
                .unwrap();

            // Then those sessions should be updated
            for session in account1_sessions {
                let updated = manager
                    .store
                    .get_inbound_group_session(session.room_id(), session.session_id())
                    .await
                    .unwrap()
                    .expect("Could not find session after update");
                assert_matches!(
                    updated.sender_data,
                    SenderData::DeviceInfo { .. },
                    "incorrect sender data for session {}",
                    session.session_id()
                );
            }

            // ... and those from the other account should not
            for session in account2_sessions {
                let updated = manager
                    .store
                    .get_inbound_group_session(session.room_id(), session.session_id())
                    .await
                    .unwrap()
                    .expect("Could not find session after update");
                assert_matches!(updated.sender_data, SenderData::UnknownDevice { .. });
            }
        }

        /// Create an InboundGroupSession sent from the given account
        async fn create_inbound_group_session(account: &Account) -> InboundGroupSession {
            let (_, igs) = account
                .create_group_session_pair(
                    room_id!("!test:room"),
                    EncryptionSettings::default(),
                    SenderData::unknown(),
                )
                .await
                .unwrap();
            igs
        }
    }
}
