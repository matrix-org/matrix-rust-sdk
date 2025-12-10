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
    sync::Arc,
    time::Duration,
};

use matrix_sdk_common::{failures_cache::FailuresCache, locks::RwLock as StdRwLock};
use ruma::{
    DeviceId, OneTimeKeyAlgorithm, OwnedDeviceId, OwnedOneTimeKeyId, OwnedServerName,
    OwnedTransactionId, OwnedUserId, SecondsSinceUnixEpoch, ServerName, TransactionId, UserId,
    api::client::keys::claim_keys::v3::{
        Request as KeysClaimRequest, Response as KeysClaimResponse,
    },
    assign,
    events::dummy::ToDeviceDummyEventContent,
};
use tracing::{debug, error, info, instrument, warn};
use vodozemac::Curve25519PublicKey;

use crate::{
    DeviceData,
    error::OlmResult,
    gossiping::GossipMachine,
    store::{Result as StoreResult, Store, types::Changes},
    types::{
        EventEncryptionAlgorithm,
        events::EventType,
        requests::{OutgoingRequest, ToDeviceRequest},
    },
};

#[derive(Debug, Clone)]
pub(crate) struct SessionManager {
    store: Store,

    /// If there is an active /keys/claim request, its details.
    ///
    /// This is used when processing the response, so that we can spot missing
    /// users/devices.
    ///
    /// According to the doc on [`crate::OlmMachine::get_missing_sessions`],
    /// there should only be one such request active at a time, so we only need
    /// to keep a record of the most recent.
    current_key_claim_request: Arc<StdRwLock<Option<(OwnedTransactionId, KeysClaimRequest)>>>,

    /// A map of user/devices that we need to automatically claim keys for.
    /// Submodules can insert user/device pairs into this map and the
    /// user/device paris will be added to the list of users when
    /// [`get_missing_sessions`](#method.get_missing_sessions) is called.
    users_for_key_claim: Arc<StdRwLock<BTreeMap<OwnedUserId, BTreeSet<OwnedDeviceId>>>>,
    wedged_devices: Arc<StdRwLock<BTreeMap<OwnedUserId, BTreeSet<OwnedDeviceId>>>>,
    key_request_machine: GossipMachine,
    outgoing_to_device_requests: Arc<StdRwLock<BTreeMap<OwnedTransactionId, OutgoingRequest>>>,

    /// Servers that have previously appeared in the `failures` section of a
    /// `/keys/claim` response.
    ///
    /// See also [`crate::identities::IdentityManager::failures`].
    failures: FailuresCache<OwnedServerName>,

    failed_devices: Arc<StdRwLock<BTreeMap<OwnedUserId, FailuresCache<OwnedDeviceId>>>>,
}

impl SessionManager {
    const KEY_CLAIM_TIMEOUT: Duration = Duration::from_secs(10);
    const UNWEDGING_INTERVAL: Duration = Duration::from_secs(60 * 60);

    pub fn new(
        users_for_key_claim: Arc<StdRwLock<BTreeMap<OwnedUserId, BTreeSet<OwnedDeviceId>>>>,
        key_request_machine: GossipMachine,
        store: Store,
    ) -> Self {
        Self {
            store,
            current_key_claim_request: Default::default(),
            key_request_machine,
            users_for_key_claim,
            wedged_devices: Default::default(),
            outgoing_to_device_requests: Default::default(),
            failures: Default::default(),
            failed_devices: Default::default(),
        }
    }

    /// Mark the outgoing request as sent.
    pub fn mark_outgoing_request_as_sent(&self, id: &TransactionId) {
        self.outgoing_to_device_requests.write().remove(id);
    }

    pub async fn mark_device_as_wedged(
        &self,
        sender: &UserId,
        curve_key: Curve25519PublicKey,
    ) -> OlmResult<()> {
        if let Some(device) = self.store.get_device_from_curve_key(sender, curve_key).await?
            && let Some(session) = device.get_most_recent_session().await?
        {
            info!(sender_key = ?curve_key, "Marking session to be unwedged");

            let creation_time = Duration::from_secs(session.creation_time.get().into());
            let now = Duration::from_secs(SecondsSinceUnixEpoch::now().get().into());

            let should_unwedge = now
                .checked_sub(creation_time)
                .map(|elapsed| elapsed > Self::UNWEDGING_INTERVAL)
                .unwrap_or(true);

            if should_unwedge {
                self.users_for_key_claim
                    .write()
                    .entry(device.user_id().to_owned())
                    .or_default()
                    .insert(device.device_id().into());
                self.wedged_devices
                    .write()
                    .entry(device.user_id().to_owned())
                    .or_default()
                    .insert(device.device_id().into());
            }
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub fn is_device_wedged(&self, device: &DeviceData) -> bool {
        self.wedged_devices
            .read()
            .get(device.user_id())
            .is_some_and(|d| d.contains(device.device_id()))
    }

    /// Check if the session was created to unwedge a Device.
    ///
    /// If the device was wedged this will queue up a dummy to-device message.
    async fn check_if_unwedged(&self, user_id: &UserId, device_id: &DeviceId) -> OlmResult<()> {
        if self.wedged_devices.write().get_mut(user_id).is_some_and(|d| d.remove(device_id))
            && let Some(device) = self.store.get_device(user_id, device_id).await?
        {
            let (_, content) = device.encrypt("m.dummy", ToDeviceDummyEventContent::new()).await?;

            let event_type = content.event_type().to_owned();

            let request = ToDeviceRequest::new(
                device.user_id(),
                device.device_id().to_owned(),
                &event_type,
                content.cast(),
            );

            let request = OutgoingRequest {
                request_id: request.txn_id.clone(),
                request: Arc::new(request.into()),
            };

            self.outgoing_to_device_requests.write().insert(request.request_id.clone(), request);
        }

        Ok(())
    }

    /// Get a key claiming request for the user/device pairs that we are
    /// missing Olm sessions for.
    ///
    /// Returns None if no key claiming request needs to be sent out.
    ///
    /// Sessions need to be established between devices so group sessions for a
    /// room can be shared with them.
    ///
    /// This should be called every time a group session needs to be shared as
    /// well as between sync calls. After a sync some devices may request room
    /// keys without us having a valid Olm session with them, making it
    /// impossible to server the room key request, thus it's necessary to check
    /// for missing sessions between sync as well.
    ///
    /// **Note**: Care should be taken that only one such request at a time is
    /// in flight, e.g. using a lock.
    ///
    /// The response of a successful key claiming requests needs to be passed to
    /// the `OlmMachine` with the [`receive_keys_claim_response`].
    ///
    /// # Arguments
    ///
    /// `users` - The list of users that we should check if we lack a session
    /// with one of their devices. This can be an empty iterator when calling
    /// this method between sync requests.
    ///
    /// [`receive_keys_claim_response`]: #method.receive_keys_claim_response
    pub async fn get_missing_sessions(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> StoreResult<Option<(OwnedTransactionId, KeysClaimRequest)>> {
        let mut missing_session_devices_by_user: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
        let mut timed_out_devices_by_user: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();

        let unfailed_users = users.filter(|u| !self.failures.contains(u.server_name()));

        // Get the current list of devices for each user.
        let devices_by_user = Box::pin(
            self.key_request_machine
                .identity_manager()
                .get_user_devices_for_encryption(unfailed_users),
        )
        .await?;

        #[derive(Debug, Default)]
        struct UserFailedDeviceInfo {
            non_olm_devices: BTreeMap<OwnedDeviceId, Vec<EventEncryptionAlgorithm>>,
            bad_key_devices: BTreeSet<OwnedDeviceId>,
        }

        let mut failed_devices_by_user: BTreeMap<_, UserFailedDeviceInfo> = BTreeMap::new();

        for (user_id, user_devices) in devices_by_user {
            for (device_id, device) in user_devices {
                if !device.supports_olm() {
                    failed_devices_by_user
                        .entry(user_id.clone())
                        .or_default()
                        .non_olm_devices
                        .insert(device_id, Vec::from(device.algorithms()));
                } else if let Some(sender_key) = device.curve25519_key() {
                    let sessions = self.store.get_sessions(&sender_key.to_base64()).await?;

                    let is_missing = if let Some(sessions) = sessions {
                        sessions.lock().await.is_empty()
                    } else {
                        true
                    };

                    let is_timed_out = self.is_user_timed_out(&user_id, &device_id);

                    if is_missing && is_timed_out {
                        timed_out_devices_by_user
                            .entry(user_id.to_owned())
                            .or_default()
                            .insert(device_id);
                    } else if is_missing && !is_timed_out {
                        missing_session_devices_by_user
                            .entry(user_id.to_owned())
                            .or_default()
                            .insert(device_id, OneTimeKeyAlgorithm::SignedCurve25519);
                    }
                } else {
                    failed_devices_by_user
                        .entry(user_id.clone())
                        .or_default()
                        .bad_key_devices
                        .insert(device_id);
                }
            }
        }

        // Add the list of sessions that for some reason automatically need to
        // create an Olm session.
        for (user, device_ids) in self.users_for_key_claim.read().iter() {
            missing_session_devices_by_user.entry(user.to_owned()).or_default().extend(
                device_ids
                    .iter()
                    .map(|device_id| (device_id.clone(), OneTimeKeyAlgorithm::SignedCurve25519)),
            );
        }

        if tracing::level_enabled!(tracing::Level::DEBUG) {
            // Reformat the map to skip the encryption algorithm, which isn't very useful.
            let missing_session_devices_by_user = missing_session_devices_by_user
                .iter()
                .map(|(user_id, devices)| (user_id, devices.keys().collect::<BTreeSet<_>>()))
                .collect::<BTreeMap<_, _>>();
            debug!(
                ?missing_session_devices_by_user,
                ?timed_out_devices_by_user,
                "Collected user/device pairs that are missing an Olm session"
            );
        }

        if !failed_devices_by_user.is_empty() {
            warn!(
                ?failed_devices_by_user,
                "Can't establish an Olm session with some devices due to missing Olm support or bad keys",
            );
        }

        let result = if missing_session_devices_by_user.is_empty() {
            None
        } else {
            Some((
                TransactionId::new(),
                assign!(KeysClaimRequest::new(missing_session_devices_by_user), {
                    timeout: Some(Self::KEY_CLAIM_TIMEOUT),
                }),
            ))
        };

        // stash the details of the request so that we can refer to it when handling the
        // response
        *(self.current_key_claim_request.write()) = result.clone();
        Ok(result)
    }

    fn is_user_timed_out(&self, user_id: &UserId, device_id: &DeviceId) -> bool {
        self.failed_devices.read().get(user_id).is_some_and(|d| d.contains(device_id))
    }

    /// This method will try to figure out for which devices a one-time key was
    /// requested but is not present in the response.
    ///
    /// As per [spec], if a user/device pair does not have any one-time keys on
    /// the homeserver, the server will just omit the user/device pair from
    /// the response:
    ///
    /// > If the homeserver could be reached, but the user or device was
    /// > unknown, no failure is recorded. Instead, the corresponding user
    /// > or device is missing from the one_time_keys result.
    ///
    /// The user/device pairs which are missing from the response are going to
    /// be put in the failures cache so we don't retry to claim a one-time
    /// key right away next time the user tries to send a message.
    ///
    /// [spec]: https://spec.matrix.org/unstable/client-server-api/#post_matrixclientv3keysclaim
    fn handle_otk_exhaustion_failure(
        &self,
        request_id: &TransactionId,
        failed_servers: &BTreeSet<OwnedServerName>,
        one_time_keys: &BTreeMap<
            &OwnedUserId,
            BTreeMap<&OwnedDeviceId, BTreeSet<&OwnedOneTimeKeyId>>,
        >,
    ) {
        // First check that the response is for the request we were expecting.
        let request = {
            let mut guard = self.current_key_claim_request.write();
            let expected_request_id = guard.as_ref().map(|e| e.0.as_ref());

            if Some(request_id) == expected_request_id {
                // We have a confirmed match. Clear the expectation, but hang onto the details
                // of the request.
                guard.take().map(|(_, request)| request)
            } else {
                warn!(
                    ?request_id,
                    ?expected_request_id,
                    "Received a `/keys/claim` response for the wrong request"
                );
                None
            }
        };

        // If we were able to pair this response with a request, look for devices that
        // were present in the request but did not elicit a successful response.
        if let Some(request) = request {
            let devices_in_response: BTreeSet<_> = one_time_keys
                .iter()
                .flat_map(|(user_id, device_key_map)| {
                    device_key_map
                        .keys()
                        .map(|device_id| (*user_id, *device_id))
                        .collect::<BTreeSet<_>>()
                })
                .collect();

            let devices_in_request: BTreeSet<(_, _)> = request
                .one_time_keys
                .iter()
                .flat_map(|(user_id, device_key_map)| {
                    device_key_map
                        .keys()
                        .map(|device_id| (user_id, device_id))
                        .collect::<BTreeSet<_>>()
                })
                .collect();

            let missing_devices: BTreeSet<_> = devices_in_request
                .difference(&devices_in_response)
                .filter(|(user_id, _)| {
                    // Skip over users whose homeservers were in the "failed servers" list: we don't
                    // want to mark individual devices as broken *as well as* the server.
                    !failed_servers.contains(user_id.server_name())
                })
                .collect();

            if !missing_devices.is_empty() {
                let mut missing_devices_by_user: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();

                for &(user_id, device_id) in missing_devices {
                    missing_devices_by_user.entry(user_id).or_default().insert(device_id.clone());
                }

                warn!(
                    ?missing_devices_by_user,
                    "Tried to create new Olm sessions, but the signed one-time key was missing for some devices",
                );

                let mut failed_devices_lock = self.failed_devices.write();

                for (user_id, device_set) in missing_devices_by_user {
                    failed_devices_lock.entry(user_id.clone()).or_default().extend(device_set);
                }
            }
        }
    }

    /// Receive a successful key claim response and create new Olm sessions with
    /// the claimed keys.
    ///
    /// # Arguments
    ///
    /// * `request_id` - The unique id of the request that was sent out. This is
    ///   needed to couple the response with the sent out request.
    ///
    /// * `response` - The response containing the claimed one-time keys.
    #[instrument(skip(self, response))]
    pub async fn receive_keys_claim_response(
        &self,
        request_id: &TransactionId,
        response: &KeysClaimResponse,
    ) -> OlmResult<()> {
        // Collect the (user_id, device_id, device_key_id) triple for logging reasons.
        let one_time_keys: BTreeMap<_, BTreeMap<_, BTreeSet<_>>> = response
            .one_time_keys
            .iter()
            .map(|(user_id, device_map)| {
                (
                    user_id,
                    device_map
                        .iter()
                        .map(|(device_id, key_map)| {
                            (device_id, key_map.keys().collect::<BTreeSet<_>>())
                        })
                        .collect::<BTreeMap<_, _>>(),
                )
            })
            .collect();

        debug!(?request_id, ?one_time_keys, failures = ?response.failures, "Received a `/keys/claim` response");

        // Collect all the servers in the `failures` field of the response.
        let failed_servers: BTreeSet<_> = response
            .failures
            .keys()
            .filter_map(|s| ServerName::parse(s).ok())
            .filter(|s| s != self.store.static_account().user_id.server_name())
            .collect();
        let successful_servers = response.one_time_keys.keys().map(|u| u.server_name());

        // Add the user/device pairs that don't have any one-time keys to the failures
        // cache.
        self.handle_otk_exhaustion_failure(request_id, &failed_servers, &one_time_keys);
        // Add the failed servers to the failures cache.
        self.failures.extend(failed_servers);
        // Remove the servers we successfully contacted from the failures cache.
        self.failures.remove(successful_servers);

        // Finally, create some 1-to-1 sessions.
        self.create_sessions(response).await
    }

    /// Create new Olm sessions for the requested devices.
    ///
    /// # Arguments
    ///
    ///  * `device_map` - a map from (user ID, device ID) pairs to key object,
    ///    for each device we should create a session for.
    pub(crate) async fn create_sessions(&self, response: &KeysClaimResponse) -> OlmResult<()> {
        struct SessionInfo {
            session_id: String,
            algorithm: EventEncryptionAlgorithm,
            fallback_key_used: bool,
        }

        #[cfg(not(tarpaulin_include))]
        impl std::fmt::Debug for SessionInfo {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(
                    f,
                    "session_id: {}, algorithm: {}, fallback_key_used: {}",
                    self.session_id, self.algorithm, self.fallback_key_used
                )
            }
        }

        let mut changes = Changes::default();
        let mut new_sessions: BTreeMap<&UserId, BTreeMap<&DeviceId, SessionInfo>> = BTreeMap::new();
        let mut store_transaction = self.store.transaction().await;

        for (user_id, user_devices) in &response.one_time_keys {
            for (device_id, key_map) in user_devices {
                let device = match self.store.get_device_data(user_id, device_id).await {
                    Ok(Some(d)) => d,
                    Ok(None) => {
                        warn!(
                            ?user_id,
                            ?device_id,
                            "Tried to create an Olm session but the device is unknown",
                        );
                        continue;
                    }
                    Err(e) => {
                        warn!(
                            ?user_id, ?device_id, error = ?e,
                            "Tried to create an Olm session, but we can't \
                            fetch the device from the store",
                        );
                        continue;
                    }
                };

                let account = store_transaction.account().await?;
                let device_keys = self.store.get_own_device().await?.as_device_keys().clone();
                let session = match account.create_outbound_session(&device, key_map, device_keys) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(
                            ?user_id, ?device_id, error = ?e,
                            "Error creating Olm session"
                        );

                        self.failed_devices
                            .write()
                            .entry(user_id.to_owned())
                            .or_default()
                            .insert(device_id.to_owned());

                        continue;
                    }
                };

                self.key_request_machine.retry_keyshare(user_id, device_id);

                if let Err(e) = self.check_if_unwedged(user_id, device_id).await {
                    error!(?user_id, ?device_id, "Error while treating an unwedged device: {e:?}");
                }

                let session_info = SessionInfo {
                    session_id: session.session_id().to_owned(),
                    algorithm: session.algorithm().await,
                    fallback_key_used: session.created_using_fallback_key,
                };

                changes.sessions.push(session);
                new_sessions.entry(user_id).or_default().insert(device_id, session_info);
            }
        }

        store_transaction.commit().await?;
        self.store.save_changes(changes).await?;
        info!(sessions = ?new_sessions, "Established new Olm sessions");

        for (user, device_map) in new_sessions {
            if let Some(user_cache) = self.failed_devices.read().get(user) {
                user_cache.remove(device_map.into_keys());
            }
        }

        let store_cache = self.store.cache().await?;
        match self.key_request_machine.collect_incoming_key_requests(&store_cache).await {
            Ok(sessions) => {
                let changes = Changes { sessions, ..Default::default() };
                self.store.save_changes(changes).await?
            }
            // We don't propagate the error here since the next sync will retry
            // this.
            Err(e) => {
                warn!(error = ?e, "Error while trying to collect the incoming secret requests")
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, iter, ops::Deref, sync::Arc, time::Duration};

    use matrix_sdk_common::{executor::spawn, locks::RwLock as StdRwLock};
    use matrix_sdk_test::{async_test, ruma_response_from_json};
    use ruma::{
        DeviceId, OwnedUserId, UserId,
        api::client::keys::claim_keys::v3::Response as KeyClaimResponse, device_id,
        owned_server_name, user_id,
    };
    use serde_json::json;
    use tokio::sync::Mutex;
    use tracing::info;

    use super::SessionManager;
    use crate::{
        gossiping::GossipMachine,
        identities::{DeviceData, IdentityManager},
        olm::{Account, PrivateCrossSigningIdentity},
        session_manager::GroupSessionCache,
        store::{
            CryptoStoreWrapper, MemoryStore, Store,
            types::{Changes, DeviceChanges, PendingChanges},
        },
        verification::VerificationMachine,
    };

    fn user_id() -> &'static UserId {
        user_id!("@example:localhost")
    }

    fn device_id() -> &'static DeviceId {
        device_id!("DEVICEID")
    }

    fn bob_account() -> Account {
        Account::with_device_id(user_id!("@bob:localhost"), device_id!("BOBDEVICE"))
    }

    fn keys_claim_with_failure() -> KeyClaimResponse {
        let response = json!({
            "one_time_keys": {},
            "failures": {
                "example.org": {
                    "errcode": "M_RESOURCE_LIMIT_EXCEEDED",
                    "error": "Not yet ready to retry",
                }
            }
        });
        ruma_response_from_json(&response)
    }

    fn keys_claim_without_failure() -> KeyClaimResponse {
        let response = json!({
            "one_time_keys": {
                "@alice:example.org": {},
            },
            "failures": {},
        });
        ruma_response_from_json(&response)
    }

    async fn session_manager_test_helper() -> (SessionManager, IdentityManager) {
        let user_id = user_id();
        let device_id = device_id();

        let account = Account::with_device_id(user_id, device_id);
        let store = Arc::new(CryptoStoreWrapper::new(user_id, device_id, MemoryStore::new()));
        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(user_id)));
        let verification = VerificationMachine::new(
            account.static_data().clone(),
            identity.clone(),
            store.clone(),
        );

        let store = Store::new(account.static_data().clone(), identity, store, verification);
        let device = DeviceData::from_account(&account);
        store.save_pending_changes(PendingChanges { account: Some(account) }).await.unwrap();
        store
            .save_changes(Changes {
                devices: DeviceChanges { new: vec![device], ..Default::default() },
                ..Default::default()
            })
            .await
            .unwrap();

        let session_cache = GroupSessionCache::new(store.clone());
        let identity_manager = IdentityManager::new(store.clone());

        let users_for_key_claim = Arc::new(StdRwLock::new(BTreeMap::new()));
        let key_request = GossipMachine::new(
            store.clone(),
            identity_manager.clone(),
            session_cache,
            users_for_key_claim.clone(),
        );

        (SessionManager::new(users_for_key_claim, key_request, store), identity_manager)
    }

    #[async_test]
    async fn test_session_creation() {
        let (manager, _identity_manager) = session_manager_test_helper().await;
        let mut bob = bob_account();

        let bob_device = DeviceData::from_account(&bob);

        manager.store.save_device_data(&[bob_device]).await.unwrap();

        let (txn_id, request) =
            manager.get_missing_sessions(iter::once(bob.user_id())).await.unwrap().unwrap();

        assert!(request.one_time_keys.contains_key(bob.user_id()));

        bob.generate_one_time_keys(1);
        let one_time = bob.signed_one_time_keys();
        assert!(!one_time.is_empty());
        bob.mark_keys_as_published();

        let mut one_time_keys = BTreeMap::new();
        one_time_keys
            .entry(bob.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(bob.device_id().to_owned(), one_time);

        let response = KeyClaimResponse::new(one_time_keys);

        manager.receive_keys_claim_response(&txn_id, &response).await.unwrap();

        assert!(manager.get_missing_sessions(iter::once(bob.user_id())).await.unwrap().is_none());
    }

    #[async_test]
    async fn test_session_creation_waits_for_keys_query() {
        let (manager, identity_manager) = session_manager_test_helper().await;

        // start a `/keys/query` request. At this point, we are only interested in our
        // own devices.
        let (key_query_txn_id, key_query_request) =
            identity_manager.users_for_key_query().await.unwrap().pop_first().unwrap();
        info!("Initial key query: {:?}", key_query_request);

        // now bob turns up, and we start tracking his devices...
        let bob = bob_account();
        let bob_device = DeviceData::from_account(&bob);
        {
            let cache = manager.store.cache().await.unwrap();
            identity_manager
                .key_query_manager
                .synced(&cache)
                .await
                .unwrap()
                .update_tracked_users(iter::once(bob.user_id()))
                .await
                .unwrap();
        }

        // ... and start off an attempt to get the missing sessions. This should block
        // for now.
        let missing_sessions_task = {
            let manager = manager.clone();
            let bob_user_id = bob.user_id().to_owned();

            #[allow(unknown_lints, clippy::redundant_async_block)] // false positive
            spawn(
                async move { manager.get_missing_sessions(iter::once(bob_user_id.deref())).await },
            )
        };

        // the initial `/keys/query` completes, and we start another
        let response_json =
            json!({ "device_keys": { manager.store.static_account().user_id.to_owned(): {}}});
        let response = ruma_response_from_json(&response_json);
        identity_manager.receive_keys_query_response(&key_query_txn_id, &response).await.unwrap();

        let (key_query_txn_id, key_query_request) =
            identity_manager.users_for_key_query().await.unwrap().pop_first().unwrap();
        info!("Second key query: {:?}", key_query_request);

        // that second request completes with info on bob's device
        let response_json = json!({ "device_keys": { bob.user_id(): {
            bob_device.device_id(): bob_device.as_device_keys()
        }}});
        let response = ruma_response_from_json(&response_json);
        identity_manager.receive_keys_query_response(&key_query_txn_id, &response).await.unwrap();

        // the missing_sessions_task should now finally complete, with a claim
        // including bob's device
        let (_, keys_claim_request) = missing_sessions_task.await.unwrap().unwrap().unwrap();
        info!("Key claim request: {:?}", keys_claim_request.one_time_keys);
        let bob_key_claims = keys_claim_request.one_time_keys.get(bob.user_id()).unwrap();
        assert!(bob_key_claims.contains_key(bob_device.device_id()));
    }

    #[async_test]
    async fn test_session_creation_does_not_wait_for_keys_query_on_failed_server() {
        let (manager, identity_manager) = session_manager_test_helper().await;

        // We start tracking Bob's devices.
        let other_user_id = OwnedUserId::try_from("@bob:example.com").unwrap();
        {
            let cache = manager.store.cache().await.unwrap();
            identity_manager
                .key_query_manager
                .synced(&cache)
                .await
                .unwrap()
                .update_tracked_users(iter::once(other_user_id.as_ref()))
                .await
                .unwrap();
        }

        // Do a `/keys/query` request, in which Bob's server is a failure.
        let (key_query_txn_id, _key_query_request) =
            identity_manager.users_for_key_query().await.unwrap().pop_first().unwrap();
        let response = ruma_response_from_json(
            &json!({ "device_keys": {}, "failures": { other_user_id.server_name(): "unreachable" }}),
        );
        identity_manager.receive_keys_query_response(&key_query_txn_id, &response).await.unwrap();

        // Now, an attempt to get the missing sessions should now *not* block. We use a
        // timeout so that we can detect the call blocking.
        let result = tokio::time::timeout(
            Duration::from_millis(10),
            manager.get_missing_sessions(iter::once(other_user_id.as_ref())),
        )
        .await
        .expect("get_missing_sessions blocked rather than completing quickly")
        .expect("get_missing_sessions returned an error");

        assert!(result.is_none(), "get_missing_sessions returned Some(...)");
    }

    // This test doesn't run on macos because we're modifying the session
    // creation time so we can get around the UNWEDGING_INTERVAL.
    #[async_test]
    #[cfg(target_os = "linux")]
    async fn test_session_unwedging() {
        use ruma::{SecondsSinceUnixEpoch, time::SystemTime};

        let (manager, _identity_manager) = session_manager_test_helper().await;
        let mut bob = bob_account();

        let (_, mut session) = manager
            .store
            .with_transaction(|mut tr| async {
                let manager_account = tr.account().await.unwrap();
                let res = bob.create_session_for_test_helper(manager_account).await;
                Ok((tr, res))
            })
            .await
            .unwrap();

        let bob_device = DeviceData::from_account(&bob);
        let time = SystemTime::now() - Duration::from_secs(3601);
        session.creation_time = SecondsSinceUnixEpoch::from_system_time(time).unwrap();

        let devices = std::slice::from_ref(&bob_device);
        manager.store.save_device_data(devices).await.unwrap();
        manager.store.save_sessions(&[session]).await.unwrap();

        assert!(manager.get_missing_sessions(iter::once(bob.user_id())).await.unwrap().is_none());

        let curve_key = bob_device.curve25519_key().unwrap();

        assert!(!manager.users_for_key_claim.read().contains_key(bob.user_id()));
        assert!(!manager.is_device_wedged(&bob_device));
        manager.mark_device_as_wedged(bob_device.user_id(), curve_key).await.unwrap();
        assert!(manager.is_device_wedged(&bob_device));
        assert!(manager.users_for_key_claim.read().contains_key(bob.user_id()));

        let (txn_id, request) =
            manager.get_missing_sessions(iter::once(bob.user_id())).await.unwrap().unwrap();

        assert!(request.one_time_keys.contains_key(bob.user_id()));

        bob.generate_one_time_keys(1);
        let one_time = bob.signed_one_time_keys();
        assert!(!one_time.is_empty());
        bob.mark_keys_as_published();

        let mut one_time_keys = BTreeMap::new();
        one_time_keys
            .entry(bob.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(bob.device_id().to_owned(), one_time);

        let response = KeyClaimResponse::new(one_time_keys);

        assert!(manager.outgoing_to_device_requests.read().is_empty());

        manager.receive_keys_claim_response(&txn_id, &response).await.unwrap();

        assert!(!manager.is_device_wedged(&bob_device));
        assert!(manager.get_missing_sessions(iter::once(bob.user_id())).await.unwrap().is_none());
        assert!(!manager.outgoing_to_device_requests.read().is_empty())
    }

    #[async_test]
    async fn test_failure_handling() {
        let alice = user_id!("@alice:example.org");
        let alice_account = Account::with_device_id(alice, "DEVICEID".into());
        let alice_device = DeviceData::from_account(&alice_account);

        let (manager, _identity_manager) = session_manager_test_helper().await;

        manager.store.save_device_data(&[alice_device]).await.unwrap();

        let (txn_id, users_for_key_claim) =
            manager.get_missing_sessions(iter::once(alice)).await.unwrap().unwrap();
        assert!(users_for_key_claim.one_time_keys.contains_key(alice));

        manager.receive_keys_claim_response(&txn_id, &keys_claim_with_failure()).await.unwrap();
        assert!(manager.get_missing_sessions(iter::once(alice)).await.unwrap().is_none());

        // expire the failure
        manager.failures.expire(&owned_server_name!("example.org"));

        let (txn_id, users_for_key_claim) =
            manager.get_missing_sessions(iter::once(alice)).await.unwrap().unwrap();
        assert!(users_for_key_claim.one_time_keys.contains_key(alice));

        manager.receive_keys_claim_response(&txn_id, &keys_claim_without_failure()).await.unwrap();
    }

    #[async_test]
    async fn test_failed_devices_handling() {
        // Alice is missing altogether
        test_invalid_claim_response(json!({
            "one_time_keys": {},
            "failures": {},
        }))
        .await;

        // Alice is present but with no devices
        test_invalid_claim_response(json!({
            "one_time_keys": {
                "@alice:example.org": {}
            },
            "failures": {},
        }))
        .await;

        // Alice's device is present but with no keys
        test_invalid_claim_response(json!({
            "one_time_keys": {
                "@alice:example.org": {
                    "DEVICEID": {}
                }
            },
            "failures": {},
        }))
        .await;

        // Alice's device is present with a bad signature
        test_invalid_claim_response(json!({
            "one_time_keys": {
                "@alice:example.org": {
                    "DEVICEID": {
                        "signed_curve25519:AAAAAA": {
                            "fallback": true,
                            "key": "1sra5GVo1ONz478aQybxSEeHTSo2xq0Z+Q3Yzqvp3A4",
                            "signatures": {
                                "@example:morpheus.localhost": {
                                    "ed25519:YAFLBLXAUK": "Zwk90fJhZWOYGNOgtOswZ6RSOGeTjTi/h2dMpyB0CR6EVtvTra0WJtp32ntifrxtwD710y2F3pe5Oyrm7jngCQ"
                                }
                            }
                        }
                    }
                }
            },
            "failures": {},
        })).await;
    }

    /// Helper for failed_devices_handling.
    ///
    /// Takes an invalid /keys/claim response for Alice's device DEVICEID and
    /// checks that it is handled correctly. (The device should be marked as
    /// 'failed'; and once that
    async fn test_invalid_claim_response(response_json: serde_json::Value) {
        let response = ruma_response_from_json(&response_json);

        let alice = user_id!("@alice:example.org");
        let mut alice_account = Account::with_device_id(alice, "DEVICEID".into());
        let alice_device = DeviceData::from_account(&alice_account);

        let (manager, _identity_manager) = session_manager_test_helper().await;
        manager.store.save_device_data(&[alice_device]).await.unwrap();

        // Since we don't have a session with Alice yet, the machine will try to claim
        // some keys for alice.
        let (txn_id, users_for_key_claim) =
            manager.get_missing_sessions(iter::once(alice)).await.unwrap().unwrap();
        assert!(users_for_key_claim.one_time_keys.contains_key(alice));

        // We receive a response with an invalid one-time key, this will mark Alice as
        // timed out.
        manager.receive_keys_claim_response(&txn_id, &response).await.unwrap();
        // Since alice is timed out, we won't claim keys for her.
        assert!(manager.get_missing_sessions(iter::once(alice)).await.unwrap().is_none());

        alice_account.generate_one_time_keys(1);
        let one_time = alice_account.signed_one_time_keys();
        assert!(!one_time.is_empty());

        let mut one_time_keys = BTreeMap::new();
        one_time_keys
            .entry(alice.to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(alice_account.device_id().to_owned(), one_time);

        // Now we expire Alice's timeout, and receive a valid one-time key for her.
        manager
            .failed_devices
            .write()
            .get(alice)
            .unwrap()
            .expire(&alice_account.device_id().to_owned());
        let (txn_id, users_for_key_claim) =
            manager.get_missing_sessions(iter::once(alice)).await.unwrap().unwrap();
        assert!(users_for_key_claim.one_time_keys.contains_key(alice));

        let response = KeyClaimResponse::new(one_time_keys);
        manager.receive_keys_claim_response(&txn_id, &response).await.unwrap();

        // Alice isn't timed out anymore.
        assert!(
            manager
                .failed_devices
                .read()
                .get(alice)
                .unwrap()
                .failure_count(alice_account.device_id())
                .is_none()
        );
    }
}
