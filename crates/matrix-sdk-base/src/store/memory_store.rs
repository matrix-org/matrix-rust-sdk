// Copyright 2021 The Matrix.org Foundation C.I.C.
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
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::RwLock,
};

use async_trait::async_trait;
use growable_bloom_filter::GrowableBloom;
use matrix_sdk_common::{ROOM_VERSION_FALLBACK, ROOM_VERSION_RULES_FALLBACK};
use ruma::{
    CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedMxcUri,
    OwnedRoomId, OwnedTransactionId, OwnedUserId, RoomId, TransactionId, UserId,
    canonical_json::{RedactedBecause, redact},
    events::{
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncStateEvent, GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
        presence::PresenceEvent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::member::{MembershipState, StrippedRoomMemberEvent, SyncRoomMemberEvent},
    },
    serde::Raw,
    time::Instant,
};
use tracing::{debug, instrument, warn};

use super::{
    DependentQueuedRequest, DependentQueuedRequestKind, QueuedRequestKind, Result, RoomInfo,
    RoomLoadSettings, StateChanges, StateStore, StoreError, SupportedVersionsResponse,
    TtlStoreValue, WellKnownResponse,
    send_queue::{ChildTransactionId, QueuedRequest, SentRequestKey},
    traits::ComposerDraft,
};
use crate::{
    MinimalRoomMemberEvent, RoomMemberships, StateStoreDataKey, StateStoreDataValue,
    deserialized_responses::{DisplayName, RawAnySyncOrStrippedState},
    store::{
        QueueWedgeError, StoredThreadSubscription,
        traits::{ThreadSubscriptionCatchupToken, compare_thread_subscription_bump_stamps},
    },
};

#[derive(Debug, Default)]
#[allow(clippy::type_complexity)]
struct MemoryStoreInner {
    recently_visited_rooms: HashMap<OwnedUserId, Vec<OwnedRoomId>>,
    composer_drafts: HashMap<(OwnedRoomId, Option<OwnedEventId>), ComposerDraft>,
    user_avatar_url: HashMap<OwnedUserId, OwnedMxcUri>,
    sync_token: Option<String>,
    supported_versions: Option<TtlStoreValue<SupportedVersionsResponse>>,
    well_known: Option<TtlStoreValue<Option<WellKnownResponse>>>,
    filters: HashMap<String, String>,
    utd_hook_manager_data: Option<GrowableBloom>,
    one_time_key_uploaded_error: bool,
    account_data: HashMap<GlobalAccountDataEventType, Raw<AnyGlobalAccountDataEvent>>,
    profiles: HashMap<OwnedRoomId, HashMap<OwnedUserId, MinimalRoomMemberEvent>>,
    display_names: HashMap<OwnedRoomId, HashMap<DisplayName, BTreeSet<OwnedUserId>>>,
    members: HashMap<OwnedRoomId, HashMap<OwnedUserId, MembershipState>>,
    room_info: HashMap<OwnedRoomId, RoomInfo>,
    room_state:
        HashMap<OwnedRoomId, HashMap<StateEventType, HashMap<String, Raw<AnySyncStateEvent>>>>,
    room_account_data:
        HashMap<OwnedRoomId, HashMap<RoomAccountDataEventType, Raw<AnyRoomAccountDataEvent>>>,
    stripped_room_state:
        HashMap<OwnedRoomId, HashMap<StateEventType, HashMap<String, Raw<AnyStrippedStateEvent>>>>,
    stripped_members: HashMap<OwnedRoomId, HashMap<OwnedUserId, MembershipState>>,
    presence: HashMap<OwnedUserId, Raw<PresenceEvent>>,
    room_user_receipts: HashMap<
        OwnedRoomId,
        HashMap<(String, Option<String>), HashMap<OwnedUserId, (OwnedEventId, Receipt)>>,
    >,
    room_event_receipts: HashMap<
        OwnedRoomId,
        HashMap<(String, Option<String>), HashMap<OwnedEventId, HashMap<OwnedUserId, Receipt>>>,
    >,
    custom: HashMap<Vec<u8>, Vec<u8>>,
    send_queue_events: BTreeMap<OwnedRoomId, Vec<QueuedRequest>>,
    dependent_send_queue_events: BTreeMap<OwnedRoomId, Vec<DependentQueuedRequest>>,
    seen_knock_requests: BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, OwnedUserId>>,
    thread_subscriptions: BTreeMap<OwnedRoomId, BTreeMap<OwnedEventId, StoredThreadSubscription>>,
    thread_subscriptions_catchup_tokens: Option<Vec<ThreadSubscriptionCatchupToken>>,
}

/// In-memory, non-persistent implementation of the `StateStore`.
///
/// Default if no other is configured at startup.
#[derive(Debug, Default)]
pub struct MemoryStore {
    inner: RwLock<MemoryStoreInner>,
}

impl MemoryStore {
    /// Create a new empty MemoryStore
    pub fn new() -> Self {
        Self::default()
    }

    fn get_user_room_receipt_event_impl(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Option<(OwnedEventId, Receipt)> {
        self.inner
            .read()
            .unwrap()
            .room_user_receipts
            .get(room_id)?
            .get(&(receipt_type.to_string(), thread.as_str().map(ToOwned::to_owned)))?
            .get(user_id)
            .cloned()
    }

    fn get_event_room_receipt_events_impl(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Option<Vec<(OwnedUserId, Receipt)>> {
        Some(
            self.inner
                .read()
                .unwrap()
                .room_event_receipts
                .get(room_id)?
                .get(&(receipt_type.to_string(), thread.as_str().map(ToOwned::to_owned)))?
                .get(event_id)?
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        )
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl StateStore for MemoryStore {
    type Error = StoreError;

    async fn get_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<Option<StateStoreDataValue>> {
        let inner = self.inner.read().unwrap();

        Ok(match key {
            StateStoreDataKey::SyncToken => {
                inner.sync_token.clone().map(StateStoreDataValue::SyncToken)
            }
            StateStoreDataKey::SupportedVersions => {
                inner.supported_versions.clone().map(StateStoreDataValue::SupportedVersions)
            }
            StateStoreDataKey::WellKnown => {
                inner.well_known.clone().map(StateStoreDataValue::WellKnown)
            }
            StateStoreDataKey::Filter(filter_name) => {
                inner.filters.get(filter_name).cloned().map(StateStoreDataValue::Filter)
            }
            StateStoreDataKey::UserAvatarUrl(user_id) => {
                inner.user_avatar_url.get(user_id).cloned().map(StateStoreDataValue::UserAvatarUrl)
            }
            StateStoreDataKey::RecentlyVisitedRooms(user_id) => inner
                .recently_visited_rooms
                .get(user_id)
                .cloned()
                .map(StateStoreDataValue::RecentlyVisitedRooms),
            StateStoreDataKey::UtdHookManagerData => {
                inner.utd_hook_manager_data.clone().map(StateStoreDataValue::UtdHookManagerData)
            }
            StateStoreDataKey::OneTimeKeyAlreadyUploaded => inner
                .one_time_key_uploaded_error
                .then_some(StateStoreDataValue::OneTimeKeyAlreadyUploaded),
            StateStoreDataKey::ComposerDraft(room_id, thread_root) => {
                let key = (room_id.to_owned(), thread_root.map(ToOwned::to_owned));
                inner.composer_drafts.get(&key).cloned().map(StateStoreDataValue::ComposerDraft)
            }
            StateStoreDataKey::SeenKnockRequests(room_id) => inner
                .seen_knock_requests
                .get(room_id)
                .cloned()
                .map(StateStoreDataValue::SeenKnockRequests),
            StateStoreDataKey::ThreadSubscriptionsCatchupTokens => inner
                .thread_subscriptions_catchup_tokens
                .clone()
                .map(StateStoreDataValue::ThreadSubscriptionsCatchupTokens),
        })
    }

    async fn set_kv_data(
        &self,
        key: StateStoreDataKey<'_>,
        value: StateStoreDataValue,
    ) -> Result<()> {
        let mut inner = self.inner.write().unwrap();
        match key {
            StateStoreDataKey::SyncToken => {
                inner.sync_token =
                    Some(value.into_sync_token().expect("Session data not a sync token"))
            }
            StateStoreDataKey::Filter(filter_name) => {
                inner.filters.insert(
                    filter_name.to_owned(),
                    value.into_filter().expect("Session data not a filter"),
                );
            }
            StateStoreDataKey::UserAvatarUrl(user_id) => {
                inner.user_avatar_url.insert(
                    user_id.to_owned(),
                    value.into_user_avatar_url().expect("Session data not a user avatar url"),
                );
            }
            StateStoreDataKey::RecentlyVisitedRooms(user_id) => {
                inner.recently_visited_rooms.insert(
                    user_id.to_owned(),
                    value
                        .into_recently_visited_rooms()
                        .expect("Session data not a list of recently visited rooms"),
                );
            }
            StateStoreDataKey::UtdHookManagerData => {
                inner.utd_hook_manager_data = Some(
                    value
                        .into_utd_hook_manager_data()
                        .expect("Session data not the hook manager data"),
                );
            }
            StateStoreDataKey::OneTimeKeyAlreadyUploaded => {
                inner.one_time_key_uploaded_error = true;
            }
            StateStoreDataKey::ComposerDraft(room_id, thread_root) => {
                inner.composer_drafts.insert(
                    (room_id.to_owned(), thread_root.map(ToOwned::to_owned)),
                    value.into_composer_draft().expect("Session data not a composer draft"),
                );
            }
            StateStoreDataKey::SupportedVersions => {
                inner.supported_versions = Some(
                    value
                        .into_supported_versions()
                        .expect("Session data not containing supported versions"),
                );
            }
            StateStoreDataKey::WellKnown => {
                inner.well_known =
                    Some(value.into_well_known().expect("Session data not containing well-known"));
            }
            StateStoreDataKey::SeenKnockRequests(room_id) => {
                inner.seen_knock_requests.insert(
                    room_id.to_owned(),
                    value
                        .into_seen_knock_requests()
                        .expect("Session data is not a set of seen join request ids"),
                );
            }
            StateStoreDataKey::ThreadSubscriptionsCatchupTokens => {
                inner.thread_subscriptions_catchup_tokens =
                    Some(value.into_thread_subscriptions_catchup_tokens().expect(
                        "Session data is not a list of thread subscription catchup tokens",
                    ));
            }
        }

        Ok(())
    }

    async fn remove_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<()> {
        let mut inner = self.inner.write().unwrap();
        match key {
            StateStoreDataKey::SyncToken => inner.sync_token = None,
            StateStoreDataKey::SupportedVersions => inner.supported_versions = None,
            StateStoreDataKey::WellKnown => inner.well_known = None,
            StateStoreDataKey::Filter(filter_name) => {
                inner.filters.remove(filter_name);
            }
            StateStoreDataKey::UserAvatarUrl(user_id) => {
                inner.user_avatar_url.remove(user_id);
            }
            StateStoreDataKey::RecentlyVisitedRooms(user_id) => {
                inner.recently_visited_rooms.remove(user_id);
            }
            StateStoreDataKey::UtdHookManagerData => inner.utd_hook_manager_data = None,
            StateStoreDataKey::OneTimeKeyAlreadyUploaded => {
                inner.one_time_key_uploaded_error = false
            }
            StateStoreDataKey::ComposerDraft(room_id, thread_root) => {
                let key = (room_id.to_owned(), thread_root.map(ToOwned::to_owned));
                inner.composer_drafts.remove(&key);
            }
            StateStoreDataKey::SeenKnockRequests(room_id) => {
                inner.seen_knock_requests.remove(room_id);
            }
            StateStoreDataKey::ThreadSubscriptionsCatchupTokens => {
                inner.thread_subscriptions_catchup_tokens = None;
            }
        }
        Ok(())
    }

    #[instrument(skip(self, changes))]
    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let now = Instant::now();

        let mut inner = self.inner.write().unwrap();

        if let Some(s) = &changes.sync_token {
            inner.sync_token = Some(s.to_owned());
        }

        for (room, users) in &changes.profiles_to_delete {
            let Some(room_profiles) = inner.profiles.get_mut(room) else {
                continue;
            };
            for user in users {
                room_profiles.remove(user);
            }
        }

        for (room, users) in &changes.profiles {
            for (user_id, profile) in users {
                inner
                    .profiles
                    .entry(room.clone())
                    .or_default()
                    .insert(user_id.clone(), profile.clone());
            }
        }

        for (room, map) in &changes.ambiguity_maps {
            for (display_name, display_names) in map {
                inner
                    .display_names
                    .entry(room.clone())
                    .or_default()
                    .insert(display_name.clone(), display_names.clone());
            }
        }

        for (event_type, event) in &changes.account_data {
            inner.account_data.insert(event_type.clone(), event.clone());
        }

        for (room, events) in &changes.room_account_data {
            for (event_type, event) in events {
                inner
                    .room_account_data
                    .entry(room.clone())
                    .or_default()
                    .insert(event_type.clone(), event.clone());
            }
        }

        for (room, event_types) in &changes.state {
            for (event_type, events) in event_types {
                for (state_key, raw_event) in events {
                    inner
                        .room_state
                        .entry(room.clone())
                        .or_default()
                        .entry(event_type.clone())
                        .or_default()
                        .insert(state_key.to_owned(), raw_event.clone());
                    inner.stripped_room_state.remove(room);

                    if *event_type == StateEventType::RoomMember {
                        let event =
                            match raw_event.deserialize_as_unchecked::<SyncRoomMemberEvent>() {
                                Ok(ev) => ev,
                                Err(e) => {
                                    let event_id: Option<String> =
                                        raw_event.get_field("event_id").ok().flatten();
                                    debug!(event_id, "Failed to deserialize member event: {e}");
                                    continue;
                                }
                            };

                        inner.stripped_members.remove(room);

                        inner
                            .members
                            .entry(room.clone())
                            .or_default()
                            .insert(event.state_key().to_owned(), event.membership().clone());
                    }
                }
            }
        }

        for (room_id, info) in &changes.room_infos {
            inner.room_info.insert(room_id.clone(), info.clone());
        }

        for (sender, event) in &changes.presence {
            inner.presence.insert(sender.clone(), event.clone());
        }

        for (room, event_types) in &changes.stripped_state {
            for (event_type, events) in event_types {
                for (state_key, raw_event) in events {
                    inner
                        .stripped_room_state
                        .entry(room.clone())
                        .or_default()
                        .entry(event_type.clone())
                        .or_default()
                        .insert(state_key.to_owned(), raw_event.clone());

                    if *event_type == StateEventType::RoomMember {
                        let event =
                            match raw_event.deserialize_as_unchecked::<StrippedRoomMemberEvent>() {
                                Ok(ev) => ev,
                                Err(e) => {
                                    let event_id: Option<String> =
                                        raw_event.get_field("event_id").ok().flatten();
                                    debug!(
                                        event_id,
                                        "Failed to deserialize stripped member event: {e}"
                                    );
                                    continue;
                                }
                            };

                        inner
                            .stripped_members
                            .entry(room.clone())
                            .or_default()
                            .insert(event.state_key, event.content.membership.clone());
                    }
                }
            }
        }

        for (room, content) in &changes.receipts {
            for (event_id, receipts) in &content.0 {
                for (receipt_type, receipts) in receipts {
                    for (user_id, receipt) in receipts {
                        let thread = receipt.thread.as_str().map(ToOwned::to_owned);
                        // Add the receipt to the room user receipts
                        if let Some((old_event, _)) = inner
                            .room_user_receipts
                            .entry(room.clone())
                            .or_default()
                            .entry((receipt_type.to_string(), thread.clone()))
                            .or_default()
                            .insert(user_id.clone(), (event_id.clone(), receipt.clone()))
                        {
                            // Remove the old receipt from the room event receipts
                            if let Some(receipt_map) = inner.room_event_receipts.get_mut(room)
                                && let Some(event_map) =
                                    receipt_map.get_mut(&(receipt_type.to_string(), thread.clone()))
                                && let Some(user_map) = event_map.get_mut(&old_event)
                            {
                                user_map.remove(user_id);
                            }
                        }

                        // Add the receipt to the room event receipts
                        inner
                            .room_event_receipts
                            .entry(room.clone())
                            .or_default()
                            .entry((receipt_type.to_string(), thread))
                            .or_default()
                            .entry(event_id.clone())
                            .or_default()
                            .insert(user_id.clone(), receipt.clone());
                    }
                }
            }
        }

        let make_redaction_rules = |room_info: &HashMap<OwnedRoomId, RoomInfo>, room_id| {
            room_info.get(room_id).map(|info| info.room_version_rules_or_default()).unwrap_or_else(|| {
                warn!(
                    ?room_id,
                    "Unable to get the room version rules, defaulting to rules for room version {ROOM_VERSION_FALLBACK}"
                );
                ROOM_VERSION_RULES_FALLBACK
            }).redaction
        };

        let inner = &mut *inner;
        for (room_id, redactions) in &changes.redactions {
            let mut redaction_rules = None;

            if let Some(room) = inner.room_state.get_mut(room_id) {
                for ref_room_mu in room.values_mut() {
                    for raw_evt in ref_room_mu.values_mut() {
                        if let Ok(Some(event_id)) = raw_evt.get_field::<OwnedEventId>("event_id")
                            && let Some(redaction) = redactions.get(&event_id)
                        {
                            let redacted = redact(
                                raw_evt.deserialize_as::<CanonicalJsonObject>()?,
                                redaction_rules.get_or_insert_with(|| {
                                    make_redaction_rules(&inner.room_info, room_id)
                                }),
                                Some(RedactedBecause::from_raw_event(redaction)?),
                            )
                            .map_err(StoreError::Redaction)?;
                            *raw_evt = Raw::new(&redacted)?.cast_unchecked();
                        }
                    }
                }
            }
        }

        debug!("Saved changes in {:?}", now.elapsed());

        Ok(())
    }

    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        Ok(self.inner.read().unwrap().presence.get(user_id).cloned())
    }

    async fn get_presence_events(
        &self,
        user_ids: &[OwnedUserId],
    ) -> Result<Vec<Raw<PresenceEvent>>> {
        let presence = &self.inner.read().unwrap().presence;
        Ok(user_ids.iter().filter_map(|user_id| presence.get(user_id).cloned()).collect())
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<RawAnySyncOrStrippedState>> {
        Ok(self
            .get_state_events_for_keys(room_id, event_type, &[state_key])
            .await?
            .into_iter()
            .next())
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<RawAnySyncOrStrippedState>> {
        fn get_events<T>(
            state_map: &HashMap<OwnedRoomId, HashMap<StateEventType, HashMap<String, Raw<T>>>>,
            room_id: &RoomId,
            event_type: &StateEventType,
            to_enum: fn(Raw<T>) -> RawAnySyncOrStrippedState,
        ) -> Option<Vec<RawAnySyncOrStrippedState>> {
            let state_events = state_map.get(room_id)?.get(event_type)?;
            Some(state_events.values().cloned().map(to_enum).collect())
        }

        let inner = self.inner.read().unwrap();
        Ok(get_events(
            &inner.stripped_room_state,
            room_id,
            &event_type,
            RawAnySyncOrStrippedState::Stripped,
        )
        .or_else(|| {
            get_events(&inner.room_state, room_id, &event_type, RawAnySyncOrStrippedState::Sync)
        })
        .unwrap_or_default())
    }

    async fn get_state_events_for_keys(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_keys: &[&str],
    ) -> Result<Vec<RawAnySyncOrStrippedState>, Self::Error> {
        let inner = self.inner.read().unwrap();

        if let Some(stripped_state_events) =
            inner.stripped_room_state.get(room_id).and_then(|events| events.get(&event_type))
        {
            Ok(state_keys
                .iter()
                .filter_map(|k| {
                    stripped_state_events
                        .get(*k)
                        .map(|e| RawAnySyncOrStrippedState::Stripped(e.clone()))
                })
                .collect())
        } else if let Some(sync_state_events) =
            inner.room_state.get(room_id).and_then(|events| events.get(&event_type))
        {
            Ok(state_keys
                .iter()
                .filter_map(|k| {
                    sync_state_events.get(*k).map(|e| RawAnySyncOrStrippedState::Sync(e.clone()))
                })
                .collect())
        } else {
            Ok(Vec::new())
        }
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalRoomMemberEvent>> {
        Ok(self
            .inner
            .read()
            .unwrap()
            .profiles
            .get(room_id)
            .and_then(|room_profiles| room_profiles.get(user_id))
            .cloned())
    }

    async fn get_profiles<'a>(
        &self,
        room_id: &RoomId,
        user_ids: &'a [OwnedUserId],
    ) -> Result<BTreeMap<&'a UserId, MinimalRoomMemberEvent>> {
        if user_ids.is_empty() {
            return Ok(BTreeMap::new());
        }

        let profiles = &self.inner.read().unwrap().profiles;
        let Some(room_profiles) = profiles.get(room_id) else {
            return Ok(BTreeMap::new());
        };

        Ok(user_ids
            .iter()
            .filter_map(|user_id| room_profiles.get(user_id).map(|p| (&**user_id, p.clone())))
            .collect())
    }

    #[instrument(skip(self, memberships))]
    async fn get_user_ids(
        &self,
        room_id: &RoomId,
        memberships: RoomMemberships,
    ) -> Result<Vec<OwnedUserId>> {
        /// Get the user IDs for the given room with the given memberships and
        /// stripped state.
        ///
        /// If `memberships` is empty, returns all user IDs in the room with the
        /// given stripped state.
        fn get_user_ids_inner(
            members: &HashMap<OwnedRoomId, HashMap<OwnedUserId, MembershipState>>,
            room_id: &RoomId,
            memberships: RoomMemberships,
        ) -> Vec<OwnedUserId> {
            members
                .get(room_id)
                .map(|members| {
                    members
                        .iter()
                        .filter_map(|(user_id, membership)| {
                            memberships.matches(membership).then_some(user_id)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        }
        let inner = self.inner.read().unwrap();
        let v = get_user_ids_inner(&inner.stripped_members, room_id, memberships);
        if !v.is_empty() {
            return Ok(v);
        }
        Ok(get_user_ids_inner(&inner.members, room_id, memberships))
    }

    async fn get_room_infos(&self, room_load_settings: &RoomLoadSettings) -> Result<Vec<RoomInfo>> {
        let memory_store_inner = self.inner.read().unwrap();
        let room_infos = &memory_store_inner.room_info;

        Ok(match room_load_settings {
            RoomLoadSettings::All => room_infos.values().cloned().collect(),

            RoomLoadSettings::One(room_id) => match room_infos.get(room_id) {
                Some(room_info) => vec![room_info.clone()],
                None => vec![],
            },
        })
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &DisplayName,
    ) -> Result<BTreeSet<OwnedUserId>> {
        Ok(self
            .inner
            .read()
            .unwrap()
            .display_names
            .get(room_id)
            .and_then(|room_names| room_names.get(display_name).cloned())
            .unwrap_or_default())
    }

    async fn get_users_with_display_names<'a>(
        &self,
        room_id: &RoomId,
        display_names: &'a [DisplayName],
    ) -> Result<HashMap<&'a DisplayName, BTreeSet<OwnedUserId>>> {
        if display_names.is_empty() {
            return Ok(HashMap::new());
        }

        let inner = self.inner.read().unwrap();
        let Some(room_names) = inner.display_names.get(room_id) else {
            return Ok(HashMap::new());
        };

        Ok(display_names.iter().filter_map(|n| room_names.get(n).map(|d| (n, d.clone()))).collect())
    }

    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        Ok(self.inner.read().unwrap().account_data.get(&event_type).cloned())
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        Ok(self
            .inner
            .read()
            .unwrap()
            .room_account_data
            .get(room_id)
            .and_then(|m| m.get(&event_type))
            .cloned())
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        Ok(self.get_user_room_receipt_event_impl(room_id, receipt_type, thread, user_id))
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        Ok(self
            .get_event_room_receipt_events_impl(room_id, receipt_type, thread, event_id)
            .unwrap_or_default())
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.inner.read().unwrap().custom.get(key).cloned())
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        Ok(self.inner.write().unwrap().custom.insert(key.to_vec(), value))
    }

    async fn remove_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.inner.write().unwrap().custom.remove(key))
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        let mut inner = self.inner.write().unwrap();

        inner.profiles.remove(room_id);
        inner.display_names.remove(room_id);
        inner.members.remove(room_id);
        inner.room_info.remove(room_id);
        inner.room_state.remove(room_id);
        inner.room_account_data.remove(room_id);
        inner.stripped_room_state.remove(room_id);
        inner.stripped_members.remove(room_id);
        inner.room_user_receipts.remove(room_id);
        inner.room_event_receipts.remove(room_id);
        inner.send_queue_events.remove(room_id);
        inner.dependent_send_queue_events.remove(room_id);
        inner.thread_subscriptions.remove(room_id);

        Ok(())
    }

    async fn save_send_queue_request(
        &self,
        room_id: &RoomId,
        transaction_id: OwnedTransactionId,
        created_at: MilliSecondsSinceUnixEpoch,
        kind: QueuedRequestKind,
        priority: usize,
    ) -> Result<(), Self::Error> {
        self.inner
            .write()
            .unwrap()
            .send_queue_events
            .entry(room_id.to_owned())
            .or_default()
            .push(QueuedRequest { kind, transaction_id, error: None, priority, created_at });
        Ok(())
    }

    async fn update_send_queue_request(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
        kind: QueuedRequestKind,
    ) -> Result<bool, Self::Error> {
        if let Some(entry) = self
            .inner
            .write()
            .unwrap()
            .send_queue_events
            .entry(room_id.to_owned())
            .or_default()
            .iter_mut()
            .find(|item| item.transaction_id == transaction_id)
        {
            entry.kind = kind;
            entry.error = None;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn remove_send_queue_request(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
    ) -> Result<bool, Self::Error> {
        let mut inner = self.inner.write().unwrap();
        let q = &mut inner.send_queue_events;

        let entry = q.get_mut(room_id);
        if let Some(entry) = entry {
            // Find the event by id in its room queue, and remove it if present.
            if let Some(pos) = entry.iter().position(|item| item.transaction_id == transaction_id) {
                entry.remove(pos);
                // And if this was the last event before removal, remove the entire room entry.
                if entry.is_empty() {
                    q.remove(room_id);
                }
                return Ok(true);
            }
        }

        Ok(false)
    }

    async fn load_send_queue_requests(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<QueuedRequest>, Self::Error> {
        let mut ret = self
            .inner
            .write()
            .unwrap()
            .send_queue_events
            .entry(room_id.to_owned())
            .or_default()
            .clone();
        // Inverted order of priority, use stable sort to keep insertion order.
        ret.sort_by(|lhs, rhs| rhs.priority.cmp(&lhs.priority));
        Ok(ret)
    }

    async fn update_send_queue_request_status(
        &self,
        room_id: &RoomId,
        transaction_id: &TransactionId,
        error: Option<QueueWedgeError>,
    ) -> Result<(), Self::Error> {
        if let Some(entry) = self
            .inner
            .write()
            .unwrap()
            .send_queue_events
            .entry(room_id.to_owned())
            .or_default()
            .iter_mut()
            .find(|item| item.transaction_id == transaction_id)
        {
            entry.error = error;
        }
        Ok(())
    }

    async fn load_rooms_with_unsent_requests(&self) -> Result<Vec<OwnedRoomId>, Self::Error> {
        Ok(self.inner.read().unwrap().send_queue_events.keys().cloned().collect())
    }

    async fn save_dependent_queued_request(
        &self,
        room: &RoomId,
        parent_transaction_id: &TransactionId,
        own_transaction_id: ChildTransactionId,
        created_at: MilliSecondsSinceUnixEpoch,
        content: DependentQueuedRequestKind,
    ) -> Result<(), Self::Error> {
        self.inner
            .write()
            .unwrap()
            .dependent_send_queue_events
            .entry(room.to_owned())
            .or_default()
            .push(DependentQueuedRequest {
                kind: content,
                parent_transaction_id: parent_transaction_id.to_owned(),
                own_transaction_id,
                parent_key: None,
                created_at,
            });
        Ok(())
    }

    async fn mark_dependent_queued_requests_as_ready(
        &self,
        room: &RoomId,
        parent_txn_id: &TransactionId,
        sent_parent_key: SentRequestKey,
    ) -> Result<usize, Self::Error> {
        let mut inner = self.inner.write().unwrap();
        let dependents = inner.dependent_send_queue_events.entry(room.to_owned()).or_default();
        let mut num_updated = 0;
        for d in dependents.iter_mut().filter(|item| item.parent_transaction_id == parent_txn_id) {
            d.parent_key = Some(sent_parent_key.clone());
            num_updated += 1;
        }
        Ok(num_updated)
    }

    async fn update_dependent_queued_request(
        &self,
        room: &RoomId,
        own_transaction_id: &ChildTransactionId,
        new_content: DependentQueuedRequestKind,
    ) -> Result<bool, Self::Error> {
        let mut inner = self.inner.write().unwrap();
        let dependents = inner.dependent_send_queue_events.entry(room.to_owned()).or_default();
        for d in dependents.iter_mut() {
            if d.own_transaction_id == *own_transaction_id {
                d.kind = new_content;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn remove_dependent_queued_request(
        &self,
        room: &RoomId,
        txn_id: &ChildTransactionId,
    ) -> Result<bool, Self::Error> {
        let mut inner = self.inner.write().unwrap();
        let dependents = inner.dependent_send_queue_events.entry(room.to_owned()).or_default();
        if let Some(pos) = dependents.iter().position(|item| item.own_transaction_id == *txn_id) {
            dependents.remove(pos);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn load_dependent_queued_requests(
        &self,
        room: &RoomId,
    ) -> Result<Vec<DependentQueuedRequest>, Self::Error> {
        Ok(self
            .inner
            .read()
            .unwrap()
            .dependent_send_queue_events
            .get(room)
            .cloned()
            .unwrap_or_default())
    }

    async fn upsert_thread_subscriptions(
        &self,
        updates: Vec<(&RoomId, &EventId, StoredThreadSubscription)>,
    ) -> Result<(), Self::Error> {
        let mut inner = self.inner.write().unwrap();

        for (room_id, thread_id, mut new) in updates {
            let room_subs = inner.thread_subscriptions.entry(room_id.to_owned()).or_default();

            if let Some(previous) = room_subs.get(thread_id) {
                if *previous == new {
                    continue;
                }
                if !compare_thread_subscription_bump_stamps(
                    previous.bump_stamp,
                    &mut new.bump_stamp,
                ) {
                    continue;
                }
            }

            room_subs.insert(thread_id.to_owned(), new);
        }

        Ok(())
    }

    async fn load_thread_subscription(
        &self,
        room: &RoomId,
        thread_id: &EventId,
    ) -> Result<Option<StoredThreadSubscription>, Self::Error> {
        let inner = self.inner.read().unwrap();
        Ok(inner
            .thread_subscriptions
            .get(room)
            .and_then(|subscriptions| subscriptions.get(thread_id))
            .copied())
    }

    async fn remove_thread_subscription(
        &self,
        room: &RoomId,
        thread_id: &EventId,
    ) -> Result<(), Self::Error> {
        let mut inner = self.inner.write().unwrap();

        let Some(room_subs) = inner.thread_subscriptions.get_mut(room) else {
            return Ok(());
        };

        room_subs.remove(thread_id);

        if room_subs.is_empty() {
            // If there are no more subscriptions for this room, remove the room entry.
            inner.thread_subscriptions.remove(room);
        }

        Ok(())
    }

    async fn optimize(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn get_size(&self) -> Result<Option<usize>, Self::Error> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryStore, Result, StateStore};

    async fn get_store() -> Result<impl StateStore> {
        Ok(MemoryStore::new())
    }

    statestore_integration_tests!();
}
