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
    fmt::Debug,
    ops::Deref,
    sync::{Arc, RwLock as StdRwLock},
};

use futures_util::future::join_all;
use itertools::{Either, Itertools};
use matrix_sdk_common::executor::spawn;
use ruma::{
    events::{AnyMessageLikeEventContent, ToDeviceEventType},
    serde::Raw,
    to_device::DeviceIdOrAllDevices,
    DeviceId, OwnedDeviceId, OwnedRoomId, OwnedTransactionId, OwnedUserId, RoomId, TransactionId,
    UserId,
};
use tracing::{debug, error, info, instrument, trace};

use crate::{
    error::{EventError, MegolmResult, OlmResult},
    identities::device::MaybeEncryptedRoomKey,
    olm::{InboundGroupSession, OutboundGroupSession, Session, ShareInfo, ShareState},
    store::{Changes, CryptoStoreWrapper, Result as StoreResult, Store},
    types::events::{room::encrypted::RoomEncryptedEventContent, room_key_withheld::WithheldCode},
    EncryptionSettings, OlmError, ReadOnlyDevice, ToDeviceRequest,
};

#[derive(Clone, Debug)]
pub(crate) struct GroupSessionCache {
    store: Store,
    sessions: Arc<StdRwLock<BTreeMap<OwnedRoomId, OutboundGroupSession>>>,
    /// A map from the request id to the group session that the request belongs
    /// to. Used to mark requests belonging to the session as shared.
    sessions_being_shared: Arc<StdRwLock<BTreeMap<OwnedTransactionId, OutboundGroupSession>>>,
}

impl GroupSessionCache {
    pub(crate) fn new(store: Store) -> Self {
        Self { store, sessions: Default::default(), sessions_being_shared: Default::default() }
    }

    pub(crate) fn insert(&self, session: OutboundGroupSession) {
        self.sessions.write().unwrap().insert(session.room_id().to_owned(), session);
    }

    /// Either get a session for the given room from the cache or load it from
    /// the store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room this session is used for.
    pub async fn get_or_load(&self, room_id: &RoomId) -> Option<OutboundGroupSession> {
        // Get the cached session, if there isn't one load one from the store
        // and put it in the cache.
        if let Some(s) = self.sessions.read().unwrap().get(room_id) {
            return Some(s.clone());
        }

        match self.store.get_outbound_group_session(room_id).await {
            Ok(Some(s)) => {
                {
                    let mut sessions_being_shared = self.sessions_being_shared.write().unwrap();
                    for request_id in s.pending_request_ids() {
                        sessions_being_shared.insert(request_id, s.clone());
                    }
                }

                self.sessions.write().unwrap().insert(room_id.to_owned(), s.clone());

                Some(s)
            }
            Ok(None) => None,
            Err(e) => {
                error!("Couldn't restore an outbound group session: {e:?}");
                None
            }
        }
    }

    /// Get an outbound group session for a room, if one exists.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which we should get the outbound
    /// group session.
    fn get(&self, room_id: &RoomId) -> Option<OutboundGroupSession> {
        self.sessions.read().unwrap().get(room_id).cloned()
    }

    /// Returns whether any session is withheld with the given device and code.
    fn has_session_withheld_to(&self, device: &ReadOnlyDevice, code: &WithheldCode) -> bool {
        self.sessions.read().unwrap().values().any(|s| s.is_withheld_to(device, code))
    }

    fn remove_from_being_shared(&self, id: &TransactionId) -> Option<OutboundGroupSession> {
        self.sessions_being_shared.write().unwrap().remove(id)
    }

    fn mark_as_being_shared(&self, id: OwnedTransactionId, session: OutboundGroupSession) {
        self.sessions_being_shared.write().unwrap().insert(id, session);
    }
}

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

#[derive(Debug, Clone)]
pub(crate) struct GroupSessionManager {
    /// Store for the encryption keys.
    /// Persists all the encryption keys so a client can resume the session
    /// without the need to create new keys.
    store: Store,
    /// The currently active outbound group sessions.
    sessions: GroupSessionCache,
}

impl GroupSessionManager {
    const MAX_TO_DEVICE_MESSAGES: usize = 250;

    pub fn new(store: Store) -> Self {
        Self { store: store.clone(), sessions: GroupSessionCache::new(store) }
    }

    pub async fn invalidate_group_session(&self, room_id: &RoomId) -> StoreResult<bool> {
        if let Some(s) = self.sessions.get(room_id) {
            s.invalidate_session();

            let mut changes = Changes::default();
            changes.outbound_group_sessions.push(s.clone());
            self.store.save_changes(changes).await?;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn mark_request_as_sent(&self, request_id: &TransactionId) -> StoreResult<()> {
        let Some(session) = self.sessions.remove_from_being_shared(request_id) else {
            return Ok(());
        };

        let no_olm = session.mark_request_as_sent(request_id);

        let mut changes = Changes::default();

        for (user_id, devices) in &no_olm {
            for device_id in devices {
                let device = self.store.get_device(user_id, device_id).await;

                if let Ok(Some(device)) = device {
                    device.mark_withheld_code_as_sent();
                    changes.devices.changed.push(device.inner.clone());
                } else {
                    error!(
                        ?request_id,
                        "Marking to-device no olm as sent but device not found, might \
                            have been deleted?"
                    );
                }
            }
        }

        changes.outbound_group_sessions.push(session.clone());
        self.store.save_changes(changes).await
    }

    #[cfg(test)]
    pub fn get_outbound_group_session(&self, room_id: &RoomId) -> Option<OutboundGroupSession> {
        self.sessions.get(room_id)
    }

    pub async fn encrypt(
        &self,
        room_id: &RoomId,
        event_type: &str,
        content: &Raw<AnyMessageLikeEventContent>,
    ) -> MegolmResult<Raw<RoomEncryptedEventContent>> {
        let session =
            self.sessions.get_or_load(room_id).await.expect("Session wasn't created nor shared");

        assert!(!session.expired(), "Session expired");

        let content = session.encrypt(event_type, content).await;

        let mut changes = Changes::default();
        changes.outbound_group_sessions.push(session);
        self.store.save_changes(changes).await?;

        Ok(content)
    }

    /// Create a new outbound group session.
    ///
    /// This also creates a matching inbound group session.
    pub async fn create_outbound_group_session(
        &self,
        room_id: &RoomId,
        settings: EncryptionSettings,
    ) -> OlmResult<(OutboundGroupSession, InboundGroupSession)> {
        let (outbound, inbound) = self
            .store
            .static_account()
            .create_group_session_pair(room_id, settings)
            .await
            .map_err(|_| EventError::UnsupportedAlgorithm)?;

        self.sessions.insert(outbound.clone());
        Ok((outbound, inbound))
    }

    pub async fn get_or_create_outbound_session(
        &self,
        room_id: &RoomId,
        settings: EncryptionSettings,
    ) -> OlmResult<(OutboundGroupSession, Option<InboundGroupSession>)> {
        let outbound_session = self.sessions.get_or_load(room_id).await;

        // If there is no session or the session has expired or is invalid,
        // create a new one.
        if let Some(s) = outbound_session {
            if s.expired() || s.invalidated() {
                self.create_outbound_group_session(room_id, settings)
                    .await
                    .map(|(o, i)| (o, i.into()))
            } else {
                Ok((s, None))
            }
        } else {
            self.create_outbound_group_session(room_id, settings).await.map(|(o, i)| (o, i.into()))
        }
    }

    /// Encrypt the given content for the given devices and create a to-device
    /// requests that sends the encrypted content to them.
    async fn encrypt_session_for(
        store: Arc<CryptoStoreWrapper>,
        group_session: OutboundGroupSession,
        devices: Vec<ReadOnlyDevice>,
    ) -> OlmResult<(
        OwnedTransactionId,
        ToDeviceRequest,
        BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, ShareInfo>>,
        Vec<Session>,
        Vec<(ReadOnlyDevice, WithheldCode)>,
    )> {
        // Use a named type instead of a tuple with rather long type name
        pub struct DeviceResult {
            device: ReadOnlyDevice,
            maybe_encrypted_room_key: MaybeEncryptedRoomKey,
        }

        let mut messages = BTreeMap::new();
        let mut changed_sessions = Vec::new();
        let mut share_infos = BTreeMap::new();
        let mut withheld_devices = Vec::new();

        // XXX is there a way to do this that doesn't involve cloning the
        // `Arc<CryptoStoreWrapper>` for each device?
        let encrypt = |store: Arc<CryptoStoreWrapper>,
                       device: ReadOnlyDevice,
                       session: OutboundGroupSession| async move {
            let encryption_result = device.maybe_encrypt_room_key(store.as_ref(), session).await?;

            Ok::<_, OlmError>(DeviceResult { device, maybe_encrypted_room_key: encryption_result })
        };

        let tasks: Vec<_> = devices
            .iter()
            .map(|d| spawn(encrypt(store.clone(), d.clone(), group_session.clone())))
            .collect();

        let results = join_all(tasks).await;

        for result in results {
            let result = result.expect("Encryption task panicked")?;

            match result.maybe_encrypted_room_key {
                MaybeEncryptedRoomKey::Encrypted { used_session, share_info, message } => {
                    changed_sessions.push(used_session);

                    let user_id = result.device.user_id().to_owned();
                    let device_id = result.device.device_id().to_owned();

                    messages
                        .entry(user_id.to_owned())
                        .or_insert_with(BTreeMap::new)
                        .insert(DeviceIdOrAllDevices::DeviceId(device_id.to_owned()), message);

                    share_infos
                        .entry(user_id)
                        .or_insert_with(BTreeMap::new)
                        .insert(device_id, share_info);
                }
                MaybeEncryptedRoomKey::Withheld { code } => {
                    withheld_devices.push((result.device, code));
                }
            }
        }

        let txn_id = TransactionId::new();
        let request = ToDeviceRequest {
            event_type: ToDeviceEventType::RoomEncrypted,
            txn_id: txn_id.to_owned(),
            messages,
        };

        Ok((txn_id, request, share_infos, changed_sessions, withheld_devices))
    }

    /// Given a list of user and an outbound session, return the list of users
    /// and their devices that this session should be shared with.
    ///
    /// Returns information indicating whether the session needs to be rotated
    /// and the list of users/devices that should receive or not the session
    /// (with withheld reason).
    #[instrument(skip_all)]
    pub async fn collect_session_recipients(
        &self,
        users: impl Iterator<Item = &UserId>,
        settings: &EncryptionSettings,
        outbound: &OutboundGroupSession,
    ) -> OlmResult<CollectRecipientsResult> {
        let users: BTreeSet<&UserId> = users.collect();
        let mut devices: BTreeMap<OwnedUserId, Vec<ReadOnlyDevice>> = Default::default();
        let mut withheld_devices: Vec<(ReadOnlyDevice, WithheldCode)> = Default::default();

        trace!(?users, ?settings, "Calculating group session recipients");

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

        // To protect the room history we need to rotate the session if either:
        //
        // 1. Any user left the room.
        // 2. Any of the users' devices got deleted or blacklisted.
        // 3. The history visibility changed.
        // 4. The encryption algorithm changed.
        //
        // This is calculated in the following code and stored in this variable.
        let mut should_rotate = user_left || visibility_changed || algorithm_changed;

        let own_identity =
            self.store.get_user_identity(self.store.user_id()).await?.and_then(|i| i.into_own());

        for user_id in users {
            let user_devices = self.store.get_readonly_devices_filtered(user_id).await?;

            // We only need the user identity if settings.only_allow_trusted_devices is set.
            let device_owner_identity = if settings.only_allow_trusted_devices {
                self.store.get_user_identity(user_id).await?
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
                } else if settings.only_allow_trusted_devices
                    && !d.is_verified(&own_identity, &device_owner_identity)
                {
                    Either::Right((d, WithheldCode::Unverified))
                } else {
                    Either::Left(d)
                }
            });

            // If we haven't already concluded that the session should be
            // rotated for other reasons, we also need to check whether any
            // of the devices in the session got deleted or blacklisted in the
            // meantime. If so, we should also rotate the session.
            if !should_rotate {
                // Device IDs that should receive this session
                let recipient_device_ids: BTreeSet<&DeviceId> =
                    recipients.iter().map(|d| d.device_id()).collect();

                if let Some(shared) = outbound.shared_with_set.read().unwrap().get(user_id) {
                    // Devices that received this session
                    let shared: BTreeSet<OwnedDeviceId> = shared.keys().cloned().collect();
                    let shared: BTreeSet<&DeviceId> = shared.iter().map(|d| d.as_ref()).collect();

                    // The set difference between
                    //
                    // 1. Devices that had previously received the session, and
                    // 2. Devices that would now receive the session
                    //
                    // Represents newly deleted or blacklisted devices. If this
                    // set is non-empty, we must rotate.
                    let newly_deleted_or_blacklisted =
                        shared.difference(&recipient_device_ids).collect::<BTreeSet<_>>();

                    should_rotate = !newly_deleted_or_blacklisted.is_empty();
                    if should_rotate {
                        debug!(
                            "Rotating a room key due to these devices being deleted/blacklisted {:?}",
                            newly_deleted_or_blacklisted,
                        );
                    }
                };
            }

            devices.entry(user_id.to_owned()).or_default().extend(recipients);
            withheld_devices.extend(withheld_recipients);
        }

        if should_rotate {
            debug!(
                should_rotate,
                user_left,
                visibility_changed,
                algorithm_changed,
                "Rotating room key to protect room history",
            );
        }
        trace!(should_rotate, "Done calculating group session recipients");

        Ok(CollectRecipientsResult { should_rotate, devices, withheld_devices })
    }

    async fn encrypt_request(
        store: Arc<CryptoStoreWrapper>,
        chunk: Vec<ReadOnlyDevice>,
        outbound: OutboundGroupSession,
        sessions: GroupSessionCache,
    ) -> OlmResult<(Vec<Session>, Vec<(ReadOnlyDevice, WithheldCode)>)> {
        let (id, request, share_infos, used_sessions, no_olm) =
            Self::encrypt_session_for(store, outbound.clone(), chunk).await?;

        if !request.messages.is_empty() {
            trace!(
                recipient_count = request.message_count(),
                transaction_id = ?id,
                "Created a to-device request carrying a room_key"
            );

            outbound.add_request(id.clone(), request.into(), share_infos);
            sessions.mark_as_being_shared(id, outbound.clone());
        }

        Ok((used_sessions, no_olm))
    }

    pub(crate) fn session_cache(&self) -> GroupSessionCache {
        self.sessions.clone()
    }

    async fn maybe_rotate_group_session(
        &self,
        should_rotate: bool,
        room_id: &RoomId,
        outbound: OutboundGroupSession,
        encryption_settings: EncryptionSettings,
        changes: &mut Changes,
    ) -> OlmResult<OutboundGroupSession> {
        Ok(if should_rotate {
            let old_session_id = outbound.session_id();

            let (outbound, inbound) =
                self.create_outbound_group_session(room_id, encryption_settings).await?;
            changes.outbound_group_sessions.push(outbound.clone());
            changes.inbound_group_sessions.push(inbound);

            debug!(
                old_session_id = old_session_id,
                session_id = outbound.session_id(),
                "A user or device has left the room since we last sent a \
                message, or the encryption settings have changed. Rotating the \
                room key.",
            );

            outbound
        } else {
            outbound
        })
    }

    async fn encrypt_for_devices(
        &self,
        recipient_devices: Vec<ReadOnlyDevice>,
        group_session: &OutboundGroupSession,
        changes: &mut Changes,
    ) -> OlmResult<Vec<(ReadOnlyDevice, WithheldCode)>> {
        // If we have some recipients, log them here.
        if !recipient_devices.is_empty() {
            #[allow(unknown_lints, clippy::unwrap_or_default)] // false positive
            let recipients = recipient_devices.iter().fold(BTreeMap::new(), |mut acc, d| {
                acc.entry(d.user_id()).or_insert_with(BTreeSet::new).insert(d.device_id());
                acc
            });

            // If there are new recipients we need to persist the outbound group
            // session as the to-device requests are persisted with the session.
            changes.outbound_group_sessions = vec![group_session.clone()];

            let message_index = group_session.message_index().await;

            info!(
                ?recipients,
                message_index,
                room_id = ?group_session.room_id(),
                session_id = group_session.session_id(),
                "Trying to encrypt a room key",
            );
        }

        // Chunk the recipients out so each to-device request will contain a
        // limited amount of to-device messages.
        //
        // Create concurrent tasks for each chunk of recipients.
        let tasks: Vec<_> = recipient_devices
            .chunks(Self::MAX_TO_DEVICE_MESSAGES)
            .map(|chunk| {
                spawn(Self::encrypt_request(
                    self.store.crypto_store(),
                    chunk.to_vec(),
                    group_session.clone(),
                    self.sessions.clone(),
                ))
            })
            .collect();

        let mut withheld_devices = Vec::new();

        // Wait for all the tasks to finish up and queue up the Olm session that
        // was used to encrypt the room key to be persisted again. This is
        // needed because each encryption step will mutate the Olm session,
        // ratcheting its state forward.
        for result in join_all(tasks).await {
            let result = result.expect("Encryption task panicked");

            let (used_sessions, failed_no_olm) = result?;

            changes.sessions.extend(used_sessions);
            withheld_devices.extend(failed_no_olm);
        }

        Ok(withheld_devices)
    }

    fn is_withheld_to(
        &self,
        group_session: &OutboundGroupSession,
        device: &ReadOnlyDevice,
        code: &WithheldCode,
    ) -> bool {
        // The `m.no_olm` withheld code is special because it is supposed to be sent
        // only once for a given device. The `Device` remembers the flag if we
        // already sent a `m.no_olm` to this particular device so let's check
        // that first.
        //
        // Keep in mind that any outbound group session might want to send this code to
        // the device. So we need to check if any of our outbound group sessions
        // is attempting to send the code to the device.
        //
        // This still has a slight race where some other thread might remove the
        // outbound group session while a third is marking the device as having
        // received the code.
        //
        // Since nothing terrible happens if we do end up sending the withheld code
        // twice, and removing the race requires us to lock the store because the
        // `OutboundGroupSession` and the `Device` both interact with the flag we'll
        // leave it be.
        if code == &WithheldCode::NoOlm {
            device.was_withheld_code_sent() || self.sessions.has_session_withheld_to(device, code)
        } else {
            group_session.is_withheld_to(device, code)
        }
    }

    fn handle_withheld_devices(
        &self,
        group_session: &OutboundGroupSession,
        withheld_devices: Vec<(ReadOnlyDevice, WithheldCode)>,
    ) -> OlmResult<()> {
        // Convert a withheld code for the group session into a to-device event content.
        let to_content = |code| {
            let content = group_session.withheld_code(code);
            Raw::new(&content).expect("We can always serialize a withheld content info").cast()
        };

        // Helper to convert a chunk of device and withheld code pairs into a to-device
        // request and it's accompanying share info.
        let chunk_to_request = |chunk| {
            let mut messages = BTreeMap::new();
            let mut share_infos = BTreeMap::new();

            for (device, code) in chunk {
                let device: ReadOnlyDevice = device;
                let code: WithheldCode = code;

                let user_id = device.user_id().to_owned();
                let device_id = device.device_id().to_owned();

                let share_info = ShareInfo::new_withheld(code.to_owned());
                let content = to_content(code);

                messages
                    .entry(user_id.to_owned())
                    .or_insert_with(BTreeMap::new)
                    .insert(DeviceIdOrAllDevices::DeviceId(device_id.to_owned()), content);

                share_infos
                    .entry(user_id)
                    .or_insert_with(BTreeMap::new)
                    .insert(device_id, share_info);
            }

            let txn_id = TransactionId::new();

            let request = ToDeviceRequest {
                event_type: ToDeviceEventType::from("m.room_key.withheld"),
                txn_id,
                messages,
            };

            (request, share_infos)
        };

        let result: Vec<_> = withheld_devices
            .into_iter()
            .filter(|(device, code)| !self.is_withheld_to(group_session, device, code))
            .chunks(Self::MAX_TO_DEVICE_MESSAGES)
            .into_iter()
            .map(chunk_to_request)
            .collect();

        for (request, share_info) in result {
            if !request.messages.is_empty() {
                let txn_id = request.txn_id.to_owned();
                group_session.add_request(txn_id.to_owned(), request.into(), share_info);

                self.sessions.mark_as_being_shared(txn_id, group_session.clone());
            }
        }

        Ok(())
    }

    fn log_room_key_sharing_result(requests: &[Arc<ToDeviceRequest>]) {
        for request in requests {
            let message_list = Self::to_device_request_to_log_list(request);
            info!(
                request_id = ?request.txn_id,
                ?message_list,
                "Created batch of to-device messages of type {}",
                request.event_type
            );
        }
    }

    /// Given a to-device request, build a recipient map suitable for logging.
    ///
    /// Returns a list of triples of (message_id, user id, device_id).
    #[cfg(feature = "message-ids")]
    fn to_device_request_to_log_list(
        request: &Arc<ToDeviceRequest>,
    ) -> Vec<(String, String, String)> {
        #[derive(serde::Deserialize)]
        struct ContentStub<'a> {
            #[serde(borrow, default, rename = "org.matrix.msgid")]
            message_id: Option<&'a str>,
        }

        let mut result: Vec<(String, String, String)> = Vec::new();

        for (user_id, device_map) in &request.messages {
            for (device, content) in device_map {
                let message_id: Option<&str> = content
                    .deserialize_as::<ContentStub<'_>>()
                    .expect("We should be able to deserialize the content we generated")
                    .message_id;

                result.push((
                    message_id.unwrap_or("<undefined>").to_owned(),
                    user_id.to_string(),
                    device.to_string(),
                ));
            }
        }
        result
    }

    /// Given a to-device request, build a recipient map suitable for logging.
    ///
    /// Returns a list of pairs of (user id, device_id).
    #[cfg(not(feature = "message-ids"))]
    fn to_device_request_to_log_list(request: &Arc<ToDeviceRequest>) -> Vec<(String, String)> {
        let mut result: Vec<(String, String)> = Vec::new();

        for (user_id, device_map) in &request.messages {
            for device in device_map.keys() {
                result.push((user_id.to_string(), device.to_string()));
            }
        }
        result
    }

    /// Get to-device requests to share a room key with users in a room.
    ///
    /// # Arguments
    ///
    /// `room_id` - The room id of the room where the room key will be used.
    ///
    /// `users` - The list of users that should receive the room key.
    ///
    /// `encryption_settings` - The settings that should be used for
    /// the room key.
    #[instrument(skip(self, users, encryption_settings), fields(session_id))]
    pub async fn share_room_key(
        &self,
        room_id: &RoomId,
        users: impl Iterator<Item = &UserId>,
        encryption_settings: impl Into<EncryptionSettings>,
    ) -> OlmResult<Vec<Arc<ToDeviceRequest>>> {
        trace!("Checking if a room key needs to be shared");

        let encryption_settings = encryption_settings.into();
        let mut changes = Changes::default();

        // Try to get an existing session or create a new one.
        let (outbound, inbound) =
            self.get_or_create_outbound_session(room_id, encryption_settings.clone()).await?;
        tracing::Span::current().record("session_id", outbound.session_id());

        // Having an inbound group session here means that we created a new
        // group session pair, which we then need to store.
        if let Some(inbound) = inbound {
            changes.outbound_group_sessions.push(outbound.clone());
            changes.inbound_group_sessions.push(inbound);
        }

        // Collect the recipient devices and check if either the settings
        // or the recipient list changed in a way that requires the
        // session to be rotated.
        let CollectRecipientsResult { should_rotate, devices, mut withheld_devices } =
            self.collect_session_recipients(users, &encryption_settings, &outbound).await?;

        let outbound = self
            .maybe_rotate_group_session(
                should_rotate,
                room_id,
                outbound,
                encryption_settings,
                &mut changes,
            )
            .await?;

        // Filter out the devices that already received this room key or have a
        // to-device message already queued up.
        let devices: Vec<_> = devices
            .into_iter()
            .flat_map(|(_, d)| {
                d.into_iter()
                    .filter(|d| matches!(outbound.is_shared_with(d), ShareState::NotShared))
            })
            .collect();

        // The `encrypt_for_devices()` method adds the to-device requests that will send
        // out the room key to the `OutboundGroupSession`. It doesn't do that
        // for the m.room_key_withheld events since we might have more of those
        // coming from the `collect_session_recipients()` method. Instead they get
        // returned by the method.
        let unable_to_encrypt_devices =
            self.encrypt_for_devices(devices, &outbound, &mut changes).await?;

        // Merge the withheld recipients.
        withheld_devices.extend(unable_to_encrypt_devices);

        // Now handle and add the withheld recipients to the resulting requests to the
        // `OutboundGroupSession`.
        self.handle_withheld_devices(&outbound, withheld_devices)?;

        // The to-device requests get added to the outbound group session, this
        // way we're making sure that they are persisted and scoped to the
        // session.
        let requests = outbound.pending_requests();

        if requests.is_empty() {
            if !outbound.shared() {
                debug!("The room key doesn't need to be shared with anyone. Marking as shared.");

                outbound.mark_as_shared();
                changes.outbound_group_sessions.push(outbound.clone());
            }
        } else {
            Self::log_room_key_sharing_result(&requests)
        }

        // Persist any changes we might have collected.
        if !changes.is_empty() {
            let session_count = changes.sessions.len();

            self.store.save_changes(changes).await?;

            trace!(
                session_count = session_count,
                "Stored the changed sessions after encrypting an room key"
            );
        }

        Ok(requests)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, iter, ops::Deref, sync::Arc};

    use matrix_sdk_test::{async_test, response_from_file};
    use ruma::{
        api::{
            client::{
                keys::{claim_keys, get_keys},
                to_device::send_event_to_device::v3::Response as ToDeviceResponse,
            },
            IncomingResponse,
        },
        device_id,
        events::room::history_visibility::HistoryVisibility,
        room_id,
        to_device::DeviceIdOrAllDevices,
        user_id, DeviceId, TransactionId, UserId,
    };
    use serde_json::{json, Value};

    use crate::{
        session_manager::group_sessions::CollectRecipientsResult,
        types::{
            events::room_key_withheld::{
                RoomKeyWithheldContent, RoomKeyWithheldContent::MegolmV1AesSha2, WithheldCode,
            },
            EventEncryptionAlgorithm,
        },
        EncryptionSettings, LocalTrust, OlmMachine, ToDeviceRequest,
    };

    fn alice_id() -> &'static UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> &'static DeviceId {
        device_id!("JLAFKJWSCS")
    }

    /// Returns a /keys/query response for user "@example:localhost"
    fn keys_query_response() -> get_keys::v3::Response {
        let data = include_bytes!("../../../../benchmarks/benches/crypto_bench/keys_query.json");
        let data: Value = serde_json::from_slice(data).unwrap();
        let data = response_from_file(&data);
        get_keys::v3::Response::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    fn bob_keys_query_response() -> get_keys::v3::Response {
        let data = json!({
            "device_keys": {
                "@bob:localhost": {
                    "BOBDEVICE": {
                        "user_id": "@bob:localhost",
                        "device_id": "BOBDEVICE",
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                            "m.megolm.v1.aes-sha2",
                            "m.megolm.v2.aes-sha2"
                        ],
                        "keys": {
                            "curve25519:BOBDEVICE": "QzXDFZj0Pt5xG4r11XGSrqE4mnFOTgRM5pz7n3tzohU",
                            "ed25519:BOBDEVICE": "T7QMEXcEo/NfiC/8doVHT+2XnMm0pDpRa27bmE8PlPI"
                        },
                        "signatures": {
                            "@bob:localhost": {
                                "ed25519:BOBDEVICE": "1Ee9J02KoVf4DKhT+LkurpZJEygiznqpgkT4lqvMTLtZyzShsVTnwmoMPttuGcJkLp9lMK1egveNYCEaYP80Cw"
                            }
                        }
                    }
                }
            }
        });
        let data = response_from_file(&data);

        get_keys::v3::Response::try_from_http_response(data)
            .expect("Can't parse the `/keys/upload` response")
    }

    /// Returns a keys claim response for device `BOBDEVICE` of user
    /// `@bob:localhost`.
    fn bob_one_time_key() -> claim_keys::v3::Response {
        let data = json!({
            "failures": {},
            "one_time_keys":{
                "@bob:localhost":{
                    "BOBDEVICE":{
                      "signed_curve25519:AAAAAAAAAAA": {
                          "key":"bm1olfbksjC5SwKxCLLK4XaINCA0FwR/155J85gIpCk",
                          "signatures":{
                              "@bob:localhost":{
                                  "ed25519:BOBDEVICE":"BKyS/+EV76zdZkWgny2D0svZ0ycS3etfyHCrsDgm7MYe166HqQmSoX29HsjGLvE/5F+Sg2zW7RJileUvquPwDA"
                              }
                          }
                      }
                    }
                }
            }
        });
        let data = response_from_file(&data);

        claim_keys::v3::Response::try_from_http_response(data)
            .expect("Can't parse the keys claim response")
    }

    /// Returns a key claim response for device `NMMBNBUSNR` of user
    /// `@example2:localhost`
    fn keys_claim_response() -> claim_keys::v3::Response {
        let data = include_bytes!("../../../../benchmarks/benches/crypto_bench/keys_claim.json");
        let data: Value = serde_json::from_slice(data).unwrap();
        let data = response_from_file(&data);
        claim_keys::v3::Response::try_from_http_response(data)
            .expect("Can't parse the keys claim response")
    }

    async fn machine_with_user_test_helper(user_id: &UserId, device_id: &DeviceId) -> OlmMachine {
        let keys_query = keys_query_response();
        let txn_id = TransactionId::new();

        let machine = OlmMachine::new(user_id, device_id).await;

        // complete a /keys/query and /keys/claim for @example:localhost
        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();
        let (txn_id, _keys_claim_request) = machine
            .get_missing_sessions(iter::once(user_id!("@example:localhost")))
            .await
            .unwrap()
            .unwrap();
        let keys_claim = keys_claim_response();
        machine.mark_request_as_sent(&txn_id, &keys_claim).await.unwrap();

        // complete a /keys/query and /keys/claim for @bob:localhost
        machine.mark_request_as_sent(&txn_id, &bob_keys_query_response()).await.unwrap();
        let (txn_id, _keys_claim_request) = machine
            .get_missing_sessions(iter::once(user_id!("@bob:localhost")))
            .await
            .unwrap()
            .unwrap();
        machine.mark_request_as_sent(&txn_id, &bob_one_time_key()).await.unwrap();

        machine
    }

    async fn machine() -> OlmMachine {
        machine_with_user_test_helper(alice_id(), alice_device_id()).await
    }

    async fn machine_with_shared_room_key_test_helper() -> OlmMachine {
        let machine = machine().await;
        let room_id = room_id!("!test:localhost");
        let keys_claim = keys_claim_response();

        let users = keys_claim.one_time_keys.keys().map(Deref::deref);
        let requests =
            machine.share_room_key(room_id, users, EncryptionSettings::default()).await.unwrap();

        let outbound =
            machine.inner.group_session_manager.get_outbound_group_session(room_id).unwrap();

        assert!(!outbound.pending_requests().is_empty());
        assert!(!outbound.shared());

        let response = ToDeviceResponse::new();
        for request in requests {
            machine.mark_request_as_sent(&request.txn_id, &response).await.unwrap();
        }

        assert!(outbound.shared());
        assert!(outbound.pending_requests().is_empty());

        machine
    }

    #[async_test]
    async fn test_sharing() {
        let machine = machine().await;
        let room_id = room_id!("!test:localhost");
        let keys_claim = keys_claim_response();

        let users = keys_claim.one_time_keys.keys().map(Deref::deref);

        let requests =
            machine.share_room_key(room_id, users, EncryptionSettings::default()).await.unwrap();

        let event_count: usize = requests
            .iter()
            .filter(|r| r.event_type == "m.room.encrypted".into())
            .map(|r| r.message_count())
            .sum();

        // The keys claim response has a couple of one-time keys with invalid
        // signatures, thus only 148 sessions are actually created, we check
        // that all 148 valid sessions get an room key.
        assert_eq!(event_count, 148);

        let withheld_count: usize = requests
            .iter()
            .filter(|r| r.event_type == "m.room_key.withheld".into())
            .map(|r| r.message_count())
            .sum();
        assert_eq!(withheld_count, 2);
    }

    fn count_withheld_from(requests: &[Arc<ToDeviceRequest>], code: WithheldCode) -> usize {
        requests
            .iter()
            .filter(|r| r.event_type == "m.room_key.withheld".into())
            .map(|r| {
                let mut count = 0;
                // count targets
                for message in r.messages.values() {
                    message.iter().for_each(|(_, content)| {
                        let withheld: RoomKeyWithheldContent =
                            content.deserialize_as::<RoomKeyWithheldContent>().unwrap();

                        if let MegolmV1AesSha2(content) = withheld {
                            if content.withheld_code() == code {
                                count += 1;
                            }
                        }
                    })
                }
                count
            })
            .sum()
    }

    #[async_test]
    async fn test_no_olm_sent_once() {
        let machine = machine().await;
        let keys_claim = keys_claim_response();

        let users = keys_claim.one_time_keys.keys().map(Deref::deref);

        let first_room_id = room_id!("!test:localhost");

        let requests = machine
            .share_room_key(first_room_id, users.to_owned(), EncryptionSettings::default())
            .await
            .unwrap();

        // there will be two no_olm
        let withheld_count: usize = count_withheld_from(&requests, WithheldCode::NoOlm);
        assert_eq!(withheld_count, 2);

        // Re-sharing same session while request has not been sent should not produces
        // withheld
        let new_requests = machine
            .share_room_key(first_room_id, users, EncryptionSettings::default())
            .await
            .unwrap();
        let withheld_count: usize = count_withheld_from(&new_requests, WithheldCode::NoOlm);
        // No additional request was added, still the 2 already pending
        assert_eq!(withheld_count, 2);

        let response = ToDeviceResponse::new();
        for request in requests {
            machine.mark_request_as_sent(&request.txn_id, &response).await.unwrap();
        }

        // The fact that an olm was sent should be remembered even if sharing another
        // session in an other room.
        let second_room_id = room_id!("!other:localhost");
        let users = keys_claim.one_time_keys.keys().map(Deref::deref);
        let requests = machine
            .share_room_key(second_room_id, users, EncryptionSettings::default())
            .await
            .unwrap();

        let withheld_count: usize = count_withheld_from(&requests, WithheldCode::NoOlm);
        assert_eq!(withheld_count, 0);

        // Help how do I simulate the creation of a new session for the device
        // with no session now?
    }

    #[async_test]
    async fn ratcheted_sharing() {
        let machine = machine_with_shared_room_key_test_helper().await;

        let room_id = room_id!("!test:localhost");
        let late_joiner = user_id!("@bob:localhost");
        let keys_claim = keys_claim_response();

        let mut users: BTreeSet<_> = keys_claim.one_time_keys.keys().map(Deref::deref).collect();
        users.insert(late_joiner);

        let requests = machine
            .share_room_key(room_id, users.into_iter(), EncryptionSettings::default())
            .await
            .unwrap();

        let event_count: usize = requests
            .iter()
            .filter(|r| r.event_type == "m.room.encrypted".into())
            .map(|r| r.message_count())
            .sum();
        let outbound =
            machine.inner.group_session_manager.get_outbound_group_session(room_id).unwrap();

        assert_eq!(event_count, 1);
        assert!(!outbound.pending_requests().is_empty());
    }

    #[async_test]
    async fn changing_encryption_settings() {
        let machine = machine_with_shared_room_key_test_helper().await;
        let room_id = room_id!("!test:localhost");
        let keys_claim = keys_claim_response();

        let users = keys_claim.one_time_keys.keys().map(Deref::deref);
        let outbound =
            machine.inner.group_session_manager.get_outbound_group_session(room_id).unwrap();

        let CollectRecipientsResult { should_rotate, .. } = machine
            .inner
            .group_session_manager
            .collect_session_recipients(users.clone(), &EncryptionSettings::default(), &outbound)
            .await
            .unwrap();

        assert!(!should_rotate);

        let settings = EncryptionSettings {
            history_visibility: HistoryVisibility::Invited,
            ..Default::default()
        };

        let CollectRecipientsResult { should_rotate, .. } = machine
            .inner
            .group_session_manager
            .collect_session_recipients(users.clone(), &settings, &outbound)
            .await
            .unwrap();

        assert!(should_rotate);

        let settings = EncryptionSettings {
            algorithm: EventEncryptionAlgorithm::from("m.megolm.v2.aes-sha2"),
            ..Default::default()
        };

        let CollectRecipientsResult { should_rotate, .. } = machine
            .inner
            .group_session_manager
            .collect_session_recipients(users, &settings, &outbound)
            .await
            .unwrap();

        assert!(should_rotate);
    }

    #[async_test]
    async fn test_key_recipient_collecting() {
        // The user id comes from the fact that the keys_query.json file uses
        // this one.
        let user_id = user_id!("@example:localhost");
        let device_id = device_id!("TESTDEVICE");
        let room_id = room_id!("!test:localhost");

        let machine = machine_with_user_test_helper(user_id, device_id).await;

        let (outbound, _) = machine
            .inner
            .group_session_manager
            .get_or_create_outbound_session(room_id, EncryptionSettings::default())
            .await
            .expect("We should be able to create a new session");
        let history_visibility = HistoryVisibility::Joined;
        let settings = EncryptionSettings { history_visibility, ..Default::default() };

        let users = [user_id].into_iter();

        let CollectRecipientsResult { devices: recipients, .. } = machine
            .inner
            .group_session_manager
            .collect_session_recipients(users, &settings, &outbound)
            .await
            .expect("We should be able to collect the session recipients");

        assert!(!recipients[user_id].is_empty());

        // Make sure that our own device isn't part of the recipients.
        assert!(!recipients[user_id]
            .iter()
            .any(|d| d.user_id() == user_id && d.device_id() == device_id));

        let settings =
            EncryptionSettings { only_allow_trusted_devices: true, ..Default::default() };
        let users = [user_id].into_iter();

        let CollectRecipientsResult { devices: recipients, .. } = machine
            .inner
            .group_session_manager
            .collect_session_recipients(users, &settings, &outbound)
            .await
            .expect("We should be able to collect the session recipients");

        assert!(recipients[user_id].is_empty());

        let device_id = "AFGUOBTZWM".into();
        let device = machine.get_device(user_id, device_id, None).await.unwrap().unwrap();
        device.set_local_trust(LocalTrust::Verified).await.unwrap();
        let users = [user_id].into_iter();

        let CollectRecipientsResult { devices: recipients, withheld_devices: withheld, .. } =
            machine
                .inner
                .group_session_manager
                .collect_session_recipients(users, &settings, &outbound)
                .await
                .expect("We should be able to collect the session recipients");

        assert!(recipients[user_id]
            .iter()
            .any(|d| d.user_id() == user_id && d.device_id() == device_id));

        let devices = machine.get_user_devices(user_id, None).await.unwrap();
        devices
            .devices()
            // Ignore our own device
            .filter(|d| d.device_id() != device_id!("TESTDEVICE"))
            .for_each(|d| {
                if d.is_blacklisted() {
                    assert!(withheld.iter().any(|(dev, w)| {
                        dev.device_id() == d.device_id() && w == &WithheldCode::Blacklisted
                    }));
                } else if !d.is_verified() {
                    // the device should then be in the list of withhelds
                    assert!(withheld.iter().any(|(dev, w)| {
                        dev.device_id() == d.device_id() && w == &WithheldCode::Unverified
                    }));
                }
            });

        assert_eq!(149, withheld.len());
    }

    #[async_test]
    async fn test_sharing_withheld_only_trusted() {
        let machine = machine().await;
        let room_id = room_id!("!test:localhost");
        let keys_claim = keys_claim_response();

        let users = keys_claim.one_time_keys.keys().map(Deref::deref);
        let settings =
            EncryptionSettings { only_allow_trusted_devices: true, ..Default::default() };

        // Trust only one
        let user_id = user_id!("@example:localhost");
        let device_id = "MWFXPINOAO".into();
        let device = machine.get_device(user_id, device_id, None).await.unwrap().unwrap();
        device.set_local_trust(LocalTrust::Verified).await.unwrap();
        machine
            .get_device(user_id, "MWVTUXDNNM".into(), None)
            .await
            .unwrap()
            .unwrap()
            .set_local_trust(LocalTrust::BlackListed)
            .await
            .unwrap();

        let requests = machine.share_room_key(room_id, users, settings).await.unwrap();

        // One room key should be sent
        let room_key_count =
            requests.iter().filter(|r| r.event_type == "m.room.encrypted".into()).count();

        assert_eq!(1, room_key_count);

        let withheld_count =
            requests.iter().filter(|r| r.event_type == "m.room_key.withheld".into()).count();
        // Can be send in one batch
        assert_eq!(1, withheld_count);

        let event_count: usize = requests
            .iter()
            .filter(|r| r.event_type == "m.room_key.withheld".into())
            .map(|r| r.message_count())
            .sum();

        // withhelds are sent in clear so all device should be counted (even if no OTK)
        assert_eq!(event_count, 149);

        // One should be blacklisted
        let has_blacklist =
            requests.iter().filter(|r| r.event_type == "m.room_key.withheld".into()).any(|r| {
                let device_key = DeviceIdOrAllDevices::from(device_id!("MWVTUXDNNM").to_owned());
                let content = &r.messages[user_id][&device_key];
                let withheld: RoomKeyWithheldContent =
                    content.deserialize_as::<RoomKeyWithheldContent>().unwrap();
                if let MegolmV1AesSha2(content) = withheld {
                    content.withheld_code() == WithheldCode::Blacklisted
                } else {
                    false
                }
            });

        assert!(has_blacklist);
    }

    #[async_test]
    async fn test_no_olm_withheld_only_sent_once() {
        let keys_query = keys_query_response();
        let txn_id = TransactionId::new();

        let machine = OlmMachine::new(alice_id(), alice_device_id()).await;

        machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();
        machine.mark_request_as_sent(&txn_id, &bob_keys_query_response()).await.unwrap();

        let first_room = room_id!("!test:localhost");
        let second_room = room_id!("!test2:localhost");
        let bob_id = user_id!("@bob:localhost");

        let settings = EncryptionSettings::default();
        let users = [bob_id];

        let requests = machine
            .share_room_key(first_room, users.into_iter(), settings.to_owned())
            .await
            .unwrap();

        // One withheld request should be sent.
        let withheld_count =
            requests.iter().filter(|r| r.event_type == "m.room_key.withheld".into()).count();

        assert_eq!(withheld_count, 1);
        assert_eq!(requests.len(), 1);

        // On the second room key share attempt we're not sending another `m.no_olm`
        // code since the first one is taking care of this.
        let second_requests =
            machine.share_room_key(second_room, users.into_iter(), settings).await.unwrap();

        let withheld_count =
            second_requests.iter().filter(|r| r.event_type == "m.room_key.withheld".into()).count();

        assert_eq!(withheld_count, 0);
        assert_eq!(second_requests.len(), 0);

        let response = ToDeviceResponse::new();

        let device = machine.get_device(bob_id, "BOBDEVICE".into(), None).await.unwrap().unwrap();

        // The device should be marked as having the `m.no_olm` code received only after
        // the request has been marked as sent.
        assert!(!device.was_withheld_code_sent());

        for request in requests {
            machine.mark_request_as_sent(&request.txn_id, &response).await.unwrap();
        }

        let device = machine.get_device(bob_id, "BOBDEVICE".into(), None).await.unwrap().unwrap();

        assert!(device.was_withheld_code_sent());
    }
}
