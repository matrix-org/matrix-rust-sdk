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
    collections::HashSet,
    sync::{Arc, RwLock as SyncRwLock},
};

#[cfg(feature = "experimental-timeline")]
use dashmap::DashSet;
#[cfg(feature = "experimental-timeline")]
use futures_channel::mpsc;
#[cfg(feature = "experimental-timeline")]
use futures_core::stream::Stream;
use futures_util::stream::{self, StreamExt};
#[cfg(feature = "experimental-timeline")]
use matrix_sdk_common::locks::Mutex;
use ruma::{
    api::client::sync::sync_events::v3::RoomSummary as RumaSummary,
    events::{
        receipt::{Receipt, ReceiptType},
        room::{
            create::RoomCreateEventContent, encryption::RoomEncryptionEventContent,
            guest_access::GuestAccess, history_visibility::HistoryVisibility, join_rules::JoinRule,
            redaction::OriginalSyncRoomRedactionEvent, tombstone::RoomTombstoneEventContent,
        },
        tag::Tags,
        AnyRoomAccountDataEvent, AnyStrippedStateEvent, AnySyncStateEvent,
        RoomAccountDataEventType, StateEventType,
    },
    room::RoomType as CreateRoomType,
    EventId, OwnedEventId, OwnedMxcUri, OwnedRoomAliasId, OwnedUserId, RoomAliasId, RoomId,
    RoomVersionId, UserId,
};
use serde::{Deserialize, Serialize};

use super::{BaseRoomInfo, DisplayName, RoomMember};
use crate::{
    deserialized_responses::UnreadNotificationsCount,
    store::{Result as StoreResult, StateStore},
    MinimalStateEvent,
};
#[cfg(feature = "experimental-timeline")]
use crate::{
    deserialized_responses::{SyncRoomEvent, TimelineSlice},
    timeline_stream::{TimelineStreamBackward, TimelineStreamError, TimelineStreamForward},
};

/// The underlying room data structure collecting state for joined, left and
/// invited rooms.
#[derive(Debug, Clone)]
pub struct Room {
    room_id: Arc<RoomId>,
    own_user_id: Arc<UserId>,
    inner: Arc<SyncRwLock<RoomInfo>>,
    store: Arc<dyn StateStore>,
    #[cfg(feature = "experimental-timeline")]
    forward_timeline_streams: Arc<Mutex<Vec<mpsc::Sender<TimelineSlice>>>>,
    #[cfg(feature = "experimental-timeline")]
    backward_timeline_streams: Arc<Mutex<Vec<mpsc::Sender<TimelineSlice>>>>,
}

/// The room summary containing member counts and members that should be used to
/// calculate the room display name.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoomSummary {
    /// The heroes of the room, members that should be used for the room display
    /// name.
    heroes: Vec<String>,
    /// The number of members that are considered to be joined to the room.
    joined_member_count: u64,
    /// The number of members that are considered to be invited to the room.
    invited_member_count: u64,
}

/// Enum keeping track in which state the room is, e.g. if our own user is
/// joined, invited, or has left the room.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RoomType {
    /// The room is in a joined state.
    Joined,
    /// The room is in a left state.
    Left,
    /// The room is in a invited state.
    Invited,
}

impl Room {
    pub(crate) fn new(
        own_user_id: &UserId,
        store: Arc<dyn StateStore>,
        room_id: &RoomId,
        room_type: RoomType,
    ) -> Self {
        let room_info = RoomInfo {
            room_id: room_id.into(),
            room_type,
            notification_counts: Default::default(),
            summary: Default::default(),
            members_synced: false,
            last_prev_batch: None,
            base_info: BaseRoomInfo::new(),
        };

        Self::restore(own_user_id, store, room_info)
    }

    pub(crate) fn restore(
        own_user_id: &UserId,
        store: Arc<dyn StateStore>,
        room_info: RoomInfo,
    ) -> Self {
        Self {
            own_user_id: own_user_id.into(),
            room_id: room_info.room_id.clone(),
            store,
            inner: Arc::new(SyncRwLock::new(room_info)),
            #[cfg(feature = "experimental-timeline")]
            forward_timeline_streams: Default::default(),
            #[cfg(feature = "experimental-timeline")]
            backward_timeline_streams: Default::default(),
        }
    }

    /// Get the unique room id of the room.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Get our own user id.
    pub fn own_user_id(&self) -> &UserId {
        &self.own_user_id
    }

    /// Get the type of the room.
    pub fn room_type(&self) -> RoomType {
        self.inner.read().unwrap().room_type
    }

    /// Whether this room's [`RoomType`](CreateRoomType) is `m.space`.
    pub fn is_space(&self) -> bool {
        self.inner.read().unwrap().room_type().map_or(false, |t| *t == CreateRoomType::Space)
    }

    /// Get the unread notification counts.
    pub fn unread_notification_counts(&self) -> UnreadNotificationsCount {
        self.inner.read().unwrap().notification_counts
    }

    /// Check if the room has it's members fully synced.
    ///
    /// Members might be missing if lazy member loading was enabled for the
    /// sync.
    ///
    /// Returns true if no members are missing, false otherwise.
    pub fn are_members_synced(&self) -> bool {
        self.inner.read().unwrap().members_synced
    }

    /// Get the `prev_batch` token that was received from the last sync. May be
    /// `None` if the last sync contained the full room history.
    pub fn last_prev_batch(&self) -> Option<String> {
        self.inner.read().unwrap().last_prev_batch.clone()
    }

    /// Get the avatar url of this room.
    pub fn avatar_url(&self) -> Option<OwnedMxcUri> {
        self.inner
            .read()
            .unwrap()
            .base_info
            .avatar
            .as_ref()
            .and_then(|e| e.as_original().and_then(|e| e.content.url.clone()))
    }

    /// Get the canonical alias of this room.
    pub fn canonical_alias(&self) -> Option<OwnedRoomAliasId> {
        self.inner.read().unwrap().canonical_alias().map(ToOwned::to_owned)
    }

    /// Get the canonical alias of this room.
    pub fn alt_aliases(&self) -> Vec<OwnedRoomAliasId> {
        self.inner.read().unwrap().alt_aliases().to_owned()
    }

    /// Get the `m.room.create` content of this room.
    ///
    /// This usually isn't optional but some servers might not send an
    /// `m.room.create` event as the first event for a given room, thus this can
    /// be optional.
    ///
    /// It can also be redacted in current room versions, leaving only the
    /// `creator` field.
    pub fn create_content(&self) -> Option<RoomCreateEventContent> {
        self.inner
            .read()
            .unwrap()
            .base_info
            .create
            .as_ref()
            .and_then(|e| e.as_original().map(|e| e.content.clone()))
    }

    /// Is this room considered a direct message.
    pub fn is_direct(&self) -> bool {
        !self.inner.read().unwrap().base_info.dm_targets.is_empty()
    }

    /// If this room is a direct message, get the members that we're sharing the
    /// room with.
    ///
    /// *Note*: The member list might have been modified in the meantime and
    /// the targets might not even be in the room anymore. This setting should
    /// only be considered as guidance.
    pub fn direct_targets(&self) -> HashSet<OwnedUserId> {
        self.inner.read().unwrap().base_info.dm_targets.clone()
    }

    /// Is the room encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.inner.read().unwrap().is_encrypted()
    }

    /// Get the `m.room.encryption` content that enabled end to end encryption
    /// in the room.
    pub fn encryption_settings(&self) -> Option<RoomEncryptionEventContent> {
        self.inner.read().unwrap().base_info.encryption.clone()
    }

    /// Get the guest access policy of this room.
    pub fn guest_access(&self) -> GuestAccess {
        self.inner.read().unwrap().guest_access().clone()
    }

    /// Get the history visibility policy of this room.
    pub fn history_visibility(&self) -> HistoryVisibility {
        self.inner.read().unwrap().history_visibility().clone()
    }

    /// Is the room considered to be public.
    pub fn is_public(&self) -> bool {
        matches!(self.join_rule(), JoinRule::Public)
    }

    /// Get the join rule policy of this room.
    pub fn join_rule(&self) -> JoinRule {
        self.inner.read().unwrap().join_rule().clone()
    }

    /// Get the maximum power level that this room contains.
    ///
    /// This is useful if one wishes to normalize the power levels, e.g. from
    /// 0-100 where 100 would be the max power level.
    pub fn max_power_level(&self) -> i64 {
        self.inner.read().unwrap().base_info.max_power_level
    }

    /// Get the `m.room.name` of this room.
    pub fn name(&self) -> Option<String> {
        self.inner.read().unwrap().name().map(ToOwned::to_owned)
    }

    /// Has the room been tombstoned.
    pub fn is_tombstoned(&self) -> bool {
        self.inner.read().unwrap().base_info.tombstone.is_some()
    }

    /// Get the `m.room.tombstone` content of this room if there is one.
    pub fn tombstone(&self) -> Option<RoomTombstoneEventContent> {
        self.inner.read().unwrap().tombstone().cloned()
    }

    /// Get the topic of the room.
    pub fn topic(&self) -> Option<String> {
        self.inner.read().unwrap().topic().map(ToOwned::to_owned)
    }

    /// Calculate the canonical display name of the room, taking into account
    /// its name, aliases and members.
    ///
    /// The display name is calculated according to [this algorithm][spec].
    ///
    /// [spec]: <https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room>
    pub async fn display_name(&self) -> StoreResult<DisplayName> {
        self.calculate_name().await
    }

    /// Get the list of users ids that are considered to be joined members of
    /// this room.
    pub async fn joined_user_ids(&self) -> StoreResult<Vec<OwnedUserId>> {
        self.store.get_joined_user_ids(self.room_id()).await
    }

    /// Get the all `RoomMember`s of this room that are known to the store.
    pub async fn members(&self) -> StoreResult<Vec<RoomMember>> {
        let user_ids = self.store.get_user_ids(self.room_id()).await?;
        let mut members = Vec::new();

        for u in user_ids {
            let m = self.get_member(&u).await?;

            if let Some(member) = m {
                members.push(member);
            }
        }

        Ok(members)
    }

    /// Get the list of `RoomMember`s that are considered to be joined members
    /// of this room.
    pub async fn joined_members(&self) -> StoreResult<Vec<RoomMember>> {
        let joined = self.store.get_joined_user_ids(self.room_id()).await?;
        let mut members = Vec::new();

        for u in joined {
            let m = self.get_member(&u).await?;

            if let Some(member) = m {
                members.push(member);
            }
        }

        Ok(members)
    }

    /// Get the list of `RoomMember`s that are considered to be joined or
    /// invited members of this room.
    pub async fn active_members(&self) -> StoreResult<Vec<RoomMember>> {
        let joined = self.store.get_joined_user_ids(self.room_id()).await?;
        let invited = self.store.get_invited_user_ids(self.room_id()).await?;

        let mut members = Vec::new();

        for u in joined.iter().chain(&invited) {
            let m = self.get_member(u).await?;

            if let Some(member) = m {
                members.push(member);
            }
        }

        Ok(members)
    }

    async fn calculate_name(&self) -> StoreResult<DisplayName> {
        let summary = {
            let inner = self.inner.read().unwrap();

            if let Some(name) = &inner.name() {
                let name = name.trim();
                return Ok(DisplayName::Named(name.to_owned()));
            } else if let Some(alias) = inner.canonical_alias() {
                let alias = alias.alias().trim();
                return Ok(DisplayName::Aliased(alias.to_owned()));
            }
            inner.summary.clone()
        };

        let is_own_member = |m: &RoomMember| m.user_id() == &*self.own_user_id;
        let is_own_user_id = |u: &str| u == self.own_user_id().as_str();

        let members: Vec<RoomMember> = if summary.heroes.is_empty() {
            self.active_members().await?.into_iter().filter(|u| !is_own_member(u)).take(5).collect()
        } else {
            let members: Vec<_> =
                stream::iter(summary.heroes.iter().filter(|u| !is_own_user_id(u)))
                    .filter_map(|u| async move {
                        let user_id = UserId::parse(u.as_str()).ok()?;
                        self.get_member(&user_id).await.transpose()
                    })
                    .collect()
                    .await;

            let members: StoreResult<Vec<_>> = members.into_iter().collect();

            members?
        };

        let (joined, invited) = match self.room_type() {
            RoomType::Invited => {
                // when we were invited we don't have a proper summary, we have to do best
                // guessing
                (members.len() as u64, 1u64)
            }
            RoomType::Joined if summary.joined_member_count == 0 => {
                // joined but the summary is not completed yet
                (
                    (members.len() as u64) + 1, // we've taken ourselves out of the count
                    summary.invited_member_count,
                )
            }
            _ => (summary.joined_member_count, summary.invited_member_count),
        };

        tracing::debug!(
            room_id = self.room_id().as_str(),
            own_user = self.own_user_id.as_str(),
            joined, invited,
            heroes = ?members,
            "Calculating name for a room",
        );

        let inner = self.inner.read().unwrap();
        Ok(inner.base_info.calculate_room_name(joined, invited, members))
    }

    /// Clone the inner RoomInfo
    pub fn clone_info(&self) -> RoomInfo {
        (*self.inner.read().unwrap()).clone()
    }

    /// Update the summary with given RoomInfo
    pub fn update_summary(&self, summary: RoomInfo) {
        let mut inner = self.inner.write().unwrap();
        *inner = summary;
    }

    /// Get the `RoomMember` with the given `user_id`.
    ///
    /// Returns `None` if the member was never part of this room, otherwise
    /// return a `RoomMember` that can be in a joined, invited, left, banned
    /// state.
    pub async fn get_member(&self, user_id: &UserId) -> StoreResult<Option<RoomMember>> {
        let member_event =
            if let Some(m) = self.store.get_member_event(self.room_id(), user_id).await? {
                m
            } else {
                return Ok(None);
            };

        let presence =
            self.store.get_presence_event(user_id).await?.and_then(|e| e.deserialize().ok());
        let profile = self.store.get_profile(self.room_id(), user_id).await?;
        let max_power_level = self.max_power_level();
        let is_room_creator =
            self.inner.read().unwrap().creator().map(|c| c == user_id).unwrap_or(false);

        let power =
            self.store
                .get_state_event(self.room_id(), StateEventType::RoomPowerLevels, "")
                .await?
                .and_then(|e| e.deserialize().ok())
                .and_then(|e| {
                    if let AnySyncStateEvent::RoomPowerLevels(e) = e {
                        Some(e)
                    } else {
                        None
                    }
                });

        let ambiguous = self
            .store
            .get_users_with_display_name(
                self.room_id(),
                member_event
                    .original_content()
                    .and_then(|c| c.displayname.as_deref())
                    .unwrap_or_else(|| user_id.localpart()),
            )
            .await?
            .len()
            > 1;

        Ok(Some(RoomMember {
            event: Arc::new(member_event),
            profile: profile.into(),
            presence: presence.into(),
            power_levels: power.into(),
            max_power_level,
            is_room_creator,
            display_name_ambiguous: ambiguous,
        }))
    }

    /// Get the `Tags` for this room.
    pub async fn tags(&self) -> StoreResult<Option<Tags>> {
        if let Some(AnyRoomAccountDataEvent::Tag(event)) = self
            .store
            .get_room_account_data_event(self.room_id(), RoomAccountDataEventType::Tag)
            .await?
            .and_then(|r| r.deserialize().ok())
        {
            Ok(Some(event.content.tags))
        } else {
            Ok(None)
        }
    }

    /// Get the read receipt as a `EventId` and `Receipt` tuple for the given
    /// `user_id` in this room.
    pub async fn user_read_receipt(
        &self,
        user_id: &UserId,
    ) -> StoreResult<Option<(OwnedEventId, Receipt)>> {
        self.store.get_user_room_receipt_event(self.room_id(), ReceiptType::Read, user_id).await
    }

    /// Get the read receipts as a list of `UserId` and `Receipt` tuples for the
    /// given `event_id` in this room.
    pub async fn event_read_receipts(
        &self,
        event_id: &EventId,
    ) -> StoreResult<Vec<(OwnedUserId, Receipt)>> {
        self.store.get_event_room_receipt_events(self.room_id(), ReceiptType::Read, event_id).await
    }

    /// Get two stream into the timeline.
    /// First one is forward in time and the second one is backward in time.
    #[cfg(feature = "experimental-timeline")]
    pub async fn timeline(
        &self,
    ) -> StoreResult<(
        impl Stream<Item = SyncRoomEvent>,
        impl Stream<Item = Result<SyncRoomEvent, TimelineStreamError>>,
    )> {
        // We need to hold the lock while we create the stream so that we don't lose new
        // sync responses
        let mut forward_timeline_streams = self.forward_timeline_streams.lock().await;
        let mut backward_timeline_streams = self.backward_timeline_streams.lock().await;
        let sync_token = self.store.get_sync_token().await?;
        let event_ids = Arc::new(DashSet::new());

        let (backward_stream, backward_sender) = if let Some((stored_events, end_token)) =
            self.store.room_timeline(&self.room_id).await?
        {
            TimelineStreamBackward::new(event_ids.clone(), end_token, Some(stored_events))
        } else {
            TimelineStreamBackward::new(
                event_ids.clone(),
                Some(sync_token.clone().expect("Sync token exists")),
                None,
            )
        };

        backward_timeline_streams.push(backward_sender);

        let (forward_stream, forward_sender) = TimelineStreamForward::new(event_ids);
        forward_timeline_streams.push(forward_sender);

        Ok((forward_stream, backward_stream))
    }

    /// Create a stream that returns all events of the room's timeline forward
    /// in time.
    ///
    /// If you need also a backward stream you should use
    /// [`timeline`][`crate::Room::timeline`]
    #[cfg(feature = "experimental-timeline")]
    pub async fn timeline_forward(&self) -> StoreResult<impl Stream<Item = SyncRoomEvent>> {
        let mut forward_timeline_streams = self.forward_timeline_streams.lock().await;
        let event_ids = Arc::new(DashSet::new());

        let (forward_stream, forward_sender) = TimelineStreamForward::new(event_ids);
        forward_timeline_streams.push(forward_sender);

        Ok(forward_stream)
    }

    /// Create a stream that returns all events of the room's timeline backward
    /// in time.
    ///
    /// If you need also a forward stream you should use
    /// [`timeline`][`crate::Room::timeline`]
    #[cfg(feature = "experimental-timeline")]
    pub async fn timeline_backward(
        &self,
    ) -> StoreResult<impl Stream<Item = Result<SyncRoomEvent, TimelineStreamError>>> {
        let mut backward_timeline_streams = self.backward_timeline_streams.lock().await;
        let sync_token = self.store.get_sync_token().await?;
        let event_ids = Arc::new(DashSet::new());

        let (backward_stream, backward_sender) = if let Some((stored_events, end_token)) =
            self.store.room_timeline(&self.room_id).await?
        {
            TimelineStreamBackward::new(event_ids.clone(), end_token, Some(stored_events))
        } else {
            TimelineStreamBackward::new(event_ids.clone(), Some(sync_token.clone().unwrap()), None)
        };

        backward_timeline_streams.push(backward_sender);

        Ok(backward_stream)
    }

    /// Add a new timeline slice to the timeline streams.
    #[cfg(feature = "experimental-timeline")]
    pub async fn add_timeline_slice(&self, timeline: &TimelineSlice) {
        if timeline.sync {
            let mut streams = self.forward_timeline_streams.lock().await;
            let mut remaining_streams = Vec::with_capacity(streams.len());
            while let Some(mut forward) = streams.pop() {
                if !forward.is_closed() {
                    if let Err(error) = forward.try_send(timeline.clone()) {
                        if error.is_full() {
                            tracing::warn!("Drop timeline slice because the limit of the buffer for the forward stream is reached");
                        }
                    } else {
                        remaining_streams.push(forward);
                    }
                }
            }
            *streams = remaining_streams;
        } else {
            let mut streams = self.backward_timeline_streams.lock().await;
            let mut remaining_streams = Vec::with_capacity(streams.len());
            while let Some(mut backward) = streams.pop() {
                if !backward.is_closed() {
                    if let Err(error) = backward.try_send(timeline.clone()) {
                        if error.is_full() {
                            tracing::warn!("Drop timeline slice because the limit of the buffer for the backward stream is reached");
                        }
                    } else {
                        remaining_streams.push(backward);
                    }
                }
            }
            *streams = remaining_streams;
        }
    }
}

/// The underlying pure data structure for joined and left rooms.
///
/// Holds all the info needed to persist a room into the state store.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomInfo {
    /// The unique room id of the room.
    pub(crate) room_id: Arc<RoomId>,
    /// The type of the room.
    pub(crate) room_type: RoomType,
    /// The unread notifications counts.
    pub(crate) notification_counts: UnreadNotificationsCount,
    /// The summary of this room.
    pub(crate) summary: RoomSummary,
    /// Flag remembering if the room members are synced.
    pub(crate) members_synced: bool,
    /// The prev batch of this room we received during the last sync.
    pub(crate) last_prev_batch: Option<String>,
    /// Base room info which holds some basic event contents important for the
    /// room state.
    pub(crate) base_info: BaseRoomInfo,
}

impl RoomInfo {
    /// Mark this Room as joined
    pub fn mark_as_joined(&mut self) {
        self.room_type = RoomType::Joined;
    }

    /// Mark this Room as left
    pub fn mark_as_left(&mut self) {
        self.room_type = RoomType::Left;
    }

    /// Mark this Room as invited
    pub fn mark_as_invited(&mut self) {
        self.room_type = RoomType::Invited;
    }

    /// Mark this Room as having all the members synced
    pub fn mark_members_synced(&mut self) {
        self.members_synced = true;
    }

    /// Mark this Room still missing member information
    pub fn mark_members_missing(&mut self) {
        self.members_synced = false;
    }

    /// Set the `prev_batch`-token.
    /// Returns whether the token has differed and thus has been upgraded:
    /// `false` means no update was applied as the were the same
    pub fn set_prev_batch(&mut self, prev_batch: Option<&str>) -> bool {
        if self.last_prev_batch.as_deref() != prev_batch {
            self.last_prev_batch = prev_batch.map(|p| p.to_owned());
            true
        } else {
            false
        }
    }

    /// Whether this is an encrypted Room
    pub fn is_encrypted(&self) -> bool {
        self.base_info.encryption.is_some()
    }

    /// Handle the given state event.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_state_event(&mut self, event: &AnySyncStateEvent) -> bool {
        self.base_info.handle_state_event(event)
    }

    /// Handle the given stripped state event.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_stripped_state_event(&mut self, event: &AnyStrippedStateEvent) -> bool {
        self.base_info.handle_stripped_state_event(event)
    }

    /// Handle the given redaction.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_redaction(&mut self, event: &OriginalSyncRoomRedactionEvent) {
        self.base_info.handle_redaction(event);
    }

    /// Update the notifications count
    pub fn update_notification_count(&mut self, notification_counts: UnreadNotificationsCount) {
        self.notification_counts = notification_counts;
    }

    /// Update the RoomSummary
    ///
    /// Returns true if the Summary modified the info, false otherwise.
    pub fn update_summary(&mut self, summary: &RumaSummary) -> bool {
        let mut changed = false;

        if !summary.is_empty() {
            if !summary.heroes.is_empty() {
                self.summary.heroes = summary.heroes.clone();
                changed = true;
            }

            if let Some(joined) = summary.joined_member_count {
                self.summary.joined_member_count = joined.into();
                changed = true;
            }

            if let Some(invited) = summary.invited_member_count {
                self.summary.invited_member_count = invited.into();
                changed = true;
            }
        }

        changed
    }

    /// The number of active members (invited + joined) in the room.
    ///
    /// The return value is saturated at `u64::MAX`.
    pub fn active_members_count(&self) -> u64 {
        self.summary.joined_member_count.saturating_add(self.summary.invited_member_count)
    }

    /// Get the canonical alias of this room.
    pub fn canonical_alias(&self) -> Option<&RoomAliasId> {
        self.base_info.canonical_alias.as_ref()?.as_original()?.content.alias.as_deref()
    }

    /// Get the alternative aliases of this room.
    pub fn alt_aliases(&self) -> &[OwnedRoomAliasId] {
        self.base_info
            .canonical_alias
            .as_ref()
            .and_then(|ev| ev.as_original())
            .map(|ev| ev.content.alt_aliases.as_ref())
            .unwrap_or_default()
    }

    /// Get the room ID of this room.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Get the room version of this room.
    pub fn room_version(&self) -> Option<&RoomVersionId> {
        Some(&self.base_info.create.as_ref()?.as_original()?.content.room_version)
    }

    /// Get the room type of this room.
    pub fn room_type(&self) -> Option<&CreateRoomType> {
        self.base_info.create.as_ref()?.as_original()?.content.room_type.as_ref()
    }

    fn creator(&self) -> Option<&UserId> {
        Some(match self.base_info.create.as_ref()? {
            MinimalStateEvent::Original(ev) => &ev.content.creator,
            MinimalStateEvent::Redacted(ev) => &ev.content.creator,
        })
    }

    fn guest_access(&self) -> &GuestAccess {
        match &self.base_info.guest_access {
            Some(MinimalStateEvent::Original(ev)) => &ev.content.guest_access,
            _ => &GuestAccess::Forbidden,
        }
    }

    fn history_visibility(&self) -> &HistoryVisibility {
        match &self.base_info.history_visibility {
            Some(MinimalStateEvent::Original(ev)) => &ev.content.history_visibility,
            _ => &HistoryVisibility::WorldReadable,
        }
    }

    fn join_rule(&self) -> &JoinRule {
        match &self.base_info.join_rules {
            Some(MinimalStateEvent::Original(ev)) => &ev.content.join_rule,
            _ => &JoinRule::Public,
        }
    }

    fn name(&self) -> Option<&str> {
        Some(self.base_info.name.as_ref()?.as_original()?.content.name.as_ref()?.as_ref())
    }

    fn tombstone(&self) -> Option<&RoomTombstoneEventContent> {
        Some(&self.base_info.tombstone.as_ref()?.as_original()?.content)
    }

    fn topic(&self) -> Option<&str> {
        Some(&self.base_info.topic.as_ref()?.as_original()?.content.topic)
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use assign::assign;
    use ruma::{
        event_id,
        events::{
            room::{
                canonical_alias::RoomCanonicalAliasEventContent,
                member::{
                    MembershipState, OriginalSyncRoomMemberEvent, RoomMemberEventContent,
                    StrippedRoomMemberEvent, SyncRoomMemberEvent,
                },
                name::RoomNameEventContent,
            },
            StateUnsigned,
        },
        room_alias_id, room_id, user_id, MilliSecondsSinceUnixEpoch,
    };

    use super::*;
    use crate::{
        store::{MemoryStore, StateChanges},
        MinimalStateEvent, OriginalMinimalStateEvent,
    };

    fn make_room(room_type: RoomType) -> (Arc<MemoryStore>, Room) {
        let store = Arc::new(MemoryStore::new());
        let user_id = user_id!("@me:example.org");
        let room_id = room_id!("!test:localhost");

        (store.clone(), Room::new(user_id, store, room_id, room_type))
    }

    fn make_stripped_member_event(user_id: &UserId, name: &str) -> StrippedRoomMemberEvent {
        StrippedRoomMemberEvent {
            content: assign!(RoomMemberEventContent::new(MembershipState::Join), {
                displayname: Some(name.to_owned())
            }),
            sender: user_id.to_owned(),
            state_key: user_id.to_owned(),
        }
    }

    fn make_member_event(user_id: &UserId, name: &str) -> SyncRoomMemberEvent {
        SyncRoomMemberEvent::Original(OriginalSyncRoomMemberEvent {
            content: assign!(RoomMemberEventContent::new(MembershipState::Join), {
                displayname: Some(name.to_owned())
            }),
            sender: user_id.to_owned(),
            state_key: user_id.to_owned(),
            event_id: event_id!("$h29iv0s1:example.com").to_owned(),
            origin_server_ts: MilliSecondsSinceUnixEpoch(208u32.into()),
            unsigned: StateUnsigned::default(),
        })
    }

    #[tokio::test]
    async fn test_display_name_default() {
        let _ = env_logger::try_init();
        let (_, room) = make_room(RoomType::Joined);
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Empty);

        let canonical_alias_event = MinimalStateEvent::Original(OriginalMinimalStateEvent {
            content: assign!(RoomCanonicalAliasEventContent::new(), {
                alias: Some(room_alias_id!("#test:example.com").to_owned()),
            }),
            event_id: None,
        });

        let name_event = MinimalStateEvent::Original(OriginalMinimalStateEvent {
            content: RoomNameEventContent::new(Some("Test Room".try_into().unwrap())),
            event_id: None,
        });

        // has precedence
        room.inner.write().unwrap().base_info.canonical_alias = Some(canonical_alias_event.clone());
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Aliased("test".to_owned()));

        // has precedence
        room.inner.write().unwrap().base_info.name = Some(name_event.clone());
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Named("Test Room".to_owned()));

        let (_, room) = make_room(RoomType::Invited);
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Empty);

        // has precedence
        room.inner.write().unwrap().base_info.canonical_alias = Some(canonical_alias_event);
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Aliased("test".to_owned()));

        // has precedence
        room.inner.write().unwrap().base_info.name = Some(name_event);
        assert_eq!(room.display_name().await.unwrap(), DisplayName::Named("Test Room".to_owned()));
    }

    #[tokio::test]
    async fn test_display_name_dm_invited() {
        let _ = env_logger::try_init();
        let (store, room) = make_room(RoomType::Invited);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            heroes: vec![me.to_string(), matthew.to_string()],
        });

        changes.add_stripped_member(room_id, make_stripped_member_event(matthew, "Matthew"));
        changes.add_stripped_member(room_id, make_stripped_member_event(me, "Me"));
        store.save_changes(&changes).await.unwrap();

        room.inner.write().unwrap().update_summary(&summary);
        assert_eq!(
            room.display_name().await.unwrap(),
            DisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[tokio::test]
    async fn test_display_name_dm_invited_no_heroes() {
        let _ = env_logger::try_init();
        let (store, room) = make_room(RoomType::Invited);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());

        changes.add_stripped_member(room_id, make_stripped_member_event(matthew, "Matthew"));
        changes.add_stripped_member(room_id, make_stripped_member_event(me, "Me"));
        store.save_changes(&changes).await.unwrap();

        assert_eq!(
            room.display_name().await.unwrap(),
            DisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[tokio::test]
    async fn test_display_name_dm_joined() {
        let _ = env_logger::try_init();
        let (store, room) = make_room(RoomType::Joined);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            joined_member_count: Some(2u32.into()),
            heroes: vec![me.to_string(), matthew.to_string()],
        });

        changes
            .members
            .entry(room_id.to_owned())
            .or_default()
            .insert(matthew.to_owned(), make_member_event(matthew, "Matthew"));
        changes
            .members
            .entry(room_id.to_owned())
            .or_default()
            .insert(me.to_owned(), make_member_event(me, "Me"));
        store.save_changes(&changes).await.unwrap();

        room.inner.write().unwrap().update_summary(&summary);
        assert_eq!(
            room.display_name().await.unwrap(),
            DisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[tokio::test]
    async fn test_display_name_dm_joined_no_heroes() {
        let _ = env_logger::try_init();
        let (store, room) = make_room(RoomType::Joined);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());

        changes
            .members
            .entry(room_id.to_owned())
            .or_default()
            .insert(matthew.to_owned(), make_member_event(matthew, "Matthew"));
        changes
            .members
            .entry(room_id.to_owned())
            .or_default()
            .insert(me.to_owned(), make_member_event(me, "Me"));
        store.save_changes(&changes).await.unwrap();

        assert_eq!(
            room.display_name().await.unwrap(),
            DisplayName::Calculated("Matthew".to_owned())
        );
    }
    #[tokio::test]
    async fn test_display_name_dm_alone() {
        let _ = env_logger::try_init();
        let (store, room) = make_room(RoomType::Joined);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            joined_member_count: Some(1u32.into()),
            heroes: vec![me.to_string(), matthew.to_string()],
        });

        changes
            .members
            .entry(room_id.to_owned())
            .or_default()
            .insert(matthew.to_owned(), make_member_event(matthew, "Matthew"));
        changes
            .members
            .entry(room_id.to_owned())
            .or_default()
            .insert(me.to_owned(), make_member_event(me, "Me"));
        store.save_changes(&changes).await.unwrap();

        room.inner.write().unwrap().update_summary(&summary);
        assert_eq!(room.display_name().await.unwrap(), DisplayName::EmptyWas("Matthew".to_owned()));
    }
}
