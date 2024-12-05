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

#[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
use std::sync::RwLock as SyncRwLock;
use std::{
    collections::{BTreeMap, HashSet},
    mem,
    sync::{atomic::AtomicBool, Arc},
};

use bitflags::bitflags;
use eyeball::{SharedObservable, Subscriber};
use futures_util::{Stream, StreamExt};
#[cfg(feature = "experimental-sliding-sync")]
use matrix_sdk_common::deserialized_responses::TimelineEventKind;
#[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
use matrix_sdk_common::ring_buffer::RingBuffer;
#[cfg(feature = "experimental-sliding-sync")]
use ruma::events::AnySyncTimelineEvent;
use ruma::{
    api::client::sync::sync_events::v3::RoomSummary as RumaSummary,
    events::{
        call::member::{CallMemberStateKey, MembershipData},
        direct::OwnedDirectUserIdentifier,
        ignored_user_list::IgnoredUserListEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::{
            avatar::{self, RoomAvatarEventContent},
            encryption::RoomEncryptionEventContent,
            guest_access::GuestAccess,
            history_visibility::HistoryVisibility,
            join_rules::JoinRule,
            member::{MembershipState, RoomMemberEventContent},
            pinned_events::RoomPinnedEventsEventContent,
            power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
            redaction::SyncRoomRedactionEvent,
            tombstone::RoomTombstoneEventContent,
        },
        tag::{TagEventContent, Tags},
        AnyRoomAccountDataEvent, AnyStrippedStateEvent, AnySyncStateEvent,
        RoomAccountDataEventType,
    },
    room::RoomType,
    serde::Raw,
    EventId, MxcUri, OwnedEventId, OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId, OwnedUserId,
    RoomAliasId, RoomId, RoomVersionId, UserId,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, field::debug, info, instrument, warn};

use super::{
    members::MemberRoomInfo, BaseRoomInfo, RoomCreateWithCreatorEventContent, RoomDisplayName,
    RoomMember, RoomNotableTags,
};
#[cfg(feature = "experimental-sliding-sync")]
use crate::latest_event::LatestEvent;
use crate::{
    deserialized_responses::{DisplayName, MemberEvent, RawSyncOrStrippedState},
    notification_settings::RoomNotificationMode,
    read_receipts::RoomReadReceipts,
    store::{DynStateStore, Result as StoreResult, StateStoreExt},
    sync::UnreadNotificationsCount,
    Error, MinimalStateEvent, OriginalMinimalStateEvent, RoomMemberships,
};

/// Indicates that a notable update of `RoomInfo` has been applied, and why.
///
/// A room info notable update is an update that can be interested for other
/// parts of the code. This mechanism is used in coordination with
/// [`BaseClient::room_info_notable_update_receiver`][baseclient] (and
/// `Room::inner` plus `Room::room_info_notable_update_sender`) where `RoomInfo`
/// can be observed and some of its updates can be spread to listeners.
///
/// [baseclient]: crate::BaseClient::room_info_notable_update_receiver
#[derive(Debug, Clone)]
pub struct RoomInfoNotableUpdate {
    /// The room which was updated.
    pub room_id: OwnedRoomId,

    /// The reason for this update.
    pub reasons: RoomInfoNotableUpdateReasons,
}

bitflags! {
    /// The reason why a [`RoomInfoNotableUpdate`] is emitted.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct RoomInfoNotableUpdateReasons: u8 {
        /// The recency stamp of the `Room` has changed.
        const RECENCY_STAMP = 0b0000_0001;

        /// The latest event of the `Room` has changed.
        const LATEST_EVENT = 0b0000_0010;

        /// A read receipt has changed.
        const READ_RECEIPT = 0b0000_0100;

        /// The user-controlled unread marker value has changed.
        const UNREAD_MARKER = 0b0000_1000;

        /// A membership change happened for the current user.
        const MEMBERSHIP = 0b0001_0000;
    }
}

impl Default for RoomInfoNotableUpdateReasons {
    fn default() -> Self {
        Self::empty()
    }
}

/// The underlying room data structure collecting state for joined, left and
/// invited rooms.
#[derive(Debug, Clone)]
pub struct Room {
    /// The room ID.
    room_id: OwnedRoomId,

    /// Our own user ID.
    own_user_id: OwnedUserId,

    inner: SharedObservable<RoomInfo>,
    room_info_notable_update_sender: broadcast::Sender<RoomInfoNotableUpdate>,
    store: Arc<DynStateStore>,

    /// The most recent few encrypted events. When the keys come through to
    /// decrypt these, the most recent relevant one will replace
    /// `latest_event`. (We can't tell which one is relevant until
    /// they are decrypted.)
    ///
    /// Currently, these are held in Room rather than RoomInfo, because we were
    /// not sure whether holding too many of them might make the cache too
    /// slow to load on startup. Keeping them here means they are not cached
    /// to disk but held in memory.
    #[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
    pub latest_encrypted_events: Arc<SyncRwLock<RingBuffer<Raw<AnySyncTimelineEvent>>>>,
}

/// The room summary containing member counts and members that should be used to
/// calculate the room display name.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoomSummary {
    /// The heroes of the room, members that can be used as a fallback for the
    /// room's display name or avatar if these haven't been set.
    ///
    /// This was called `heroes` and contained raw `String`s of the `UserId`
    /// before. Following this it was called `heroes_user_ids` and a
    /// complimentary `heroes_names` existed too; changing the field's name
    /// helped with avoiding a migration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) room_heroes: Vec<RoomHero>,
    /// The number of members that are considered to be joined to the room.
    pub(crate) joined_member_count: u64,
    /// The number of members that are considered to be invited to the room.
    pub(crate) invited_member_count: u64,
}

/// Information about a member considered to be a room hero.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoomHero {
    /// The user id of the hero.
    pub user_id: OwnedUserId,
    /// The display name of the hero.
    pub display_name: Option<String>,
    /// The avatar url of the hero.
    pub avatar_url: Option<OwnedMxcUri>,
}

#[cfg(test)]
impl RoomSummary {
    pub(crate) fn heroes(&self) -> &[RoomHero] {
        &self.room_heroes
    }
}

/// Enum keeping track in which state the room is, e.g. if our own user is
/// joined, RoomState::Invited, or has left the room.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RoomState {
    /// The room is in a joined state.
    Joined,
    /// The room is in a left state.
    Left,
    /// The room is in an invited state.
    Invited,
    /// The room is in a knocked state.
    Knocked,
}

impl From<&MembershipState> for RoomState {
    fn from(membership_state: &MembershipState) -> Self {
        // We consider Ban, Knock and Leave to be Left, because they all mean we are not
        // in the room.
        match membership_state {
            MembershipState::Ban => Self::Left,
            MembershipState::Invite => Self::Invited,
            MembershipState::Join => Self::Joined,
            MembershipState::Knock => Self::Knocked,
            MembershipState::Leave => Self::Left,
            _ => panic!("Unexpected MembershipState: {}", membership_state),
        }
    }
}

/// The number of heroes chosen to compute a room's name, if the room didn't
/// have a name set by the users themselves.
///
/// A server must return at most 5 heroes, according to the paragraph below
/// https://spec.matrix.org/v1.10/client-server-api/#get_matrixclientv3sync (grep for "heroes"). We
/// try to behave similarly here.
const NUM_HEROES: usize = 5;

impl Room {
    /// The size of the latest_encrypted_events RingBuffer
    // SAFETY: `new_unchecked` is safe because 10 is not zero.
    #[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
    const MAX_ENCRYPTED_EVENTS: std::num::NonZeroUsize =
        unsafe { std::num::NonZeroUsize::new_unchecked(10) };

    pub(crate) fn new(
        own_user_id: &UserId,
        store: Arc<DynStateStore>,
        room_id: &RoomId,
        room_state: RoomState,
        room_info_notable_update_sender: broadcast::Sender<RoomInfoNotableUpdate>,
    ) -> Self {
        let room_info = RoomInfo::new(room_id, room_state);
        Self::restore(own_user_id, store, room_info, room_info_notable_update_sender)
    }

    pub(crate) fn restore(
        own_user_id: &UserId,
        store: Arc<DynStateStore>,
        room_info: RoomInfo,
        room_info_notable_update_sender: broadcast::Sender<RoomInfoNotableUpdate>,
    ) -> Self {
        Self {
            own_user_id: own_user_id.into(),
            room_id: room_info.room_id.clone(),
            store,
            inner: SharedObservable::new(room_info),
            #[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
            latest_encrypted_events: Arc::new(SyncRwLock::new(RingBuffer::new(
                Self::MAX_ENCRYPTED_EVENTS,
            ))),
            room_info_notable_update_sender,
        }
    }

    /// Get the unique room id of the room.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Get a copy of the room creator.
    pub fn creator(&self) -> Option<OwnedUserId> {
        self.inner.read().creator().map(ToOwned::to_owned)
    }

    /// Get our own user id.
    pub fn own_user_id(&self) -> &UserId {
        &self.own_user_id
    }

    /// Get the state of the room.
    pub fn state(&self) -> RoomState {
        self.inner.read().room_state
    }

    /// Get the previous state of the room, if it had any.
    pub fn prev_state(&self) -> Option<RoomState> {
        self.inner.read().prev_room_state
    }

    /// Whether this room's [`RoomType`] is `m.space`.
    pub fn is_space(&self) -> bool {
        self.inner.read().room_type().is_some_and(|t| *t == RoomType::Space)
    }

    /// Returns the room's type as defined in its creation event
    /// (`m.room.create`).
    pub fn room_type(&self) -> Option<RoomType> {
        self.inner.read().room_type().map(ToOwned::to_owned)
    }

    /// Get the unread notification counts.
    pub fn unread_notification_counts(&self) -> UnreadNotificationsCount {
        self.inner.read().notification_counts
    }

    /// Get the number of unread messages (computed client-side).
    ///
    /// This might be more precise than [`Self::unread_notification_counts`] for
    /// encrypted rooms.
    pub fn num_unread_messages(&self) -> u64 {
        self.inner.read().read_receipts.num_unread
    }

    /// Get the detailed information about read receipts for the room.
    pub fn read_receipts(&self) -> RoomReadReceipts {
        self.inner.read().read_receipts.clone()
    }

    /// Get the number of unread notifications (computed client-side).
    ///
    /// This might be more precise than [`Self::unread_notification_counts`] for
    /// encrypted rooms.
    pub fn num_unread_notifications(&self) -> u64 {
        self.inner.read().read_receipts.num_notifications
    }

    /// Get the number of unread mentions (computed client-side), that is,
    /// messages causing a highlight in a room.
    ///
    /// This might be more precise than [`Self::unread_notification_counts`] for
    /// encrypted rooms.
    pub fn num_unread_mentions(&self) -> u64 {
        self.inner.read().read_receipts.num_mentions
    }

    /// Check if the room has its members fully synced.
    ///
    /// Members might be missing if lazy member loading was enabled for the
    /// sync.
    ///
    /// Returns true if no members are missing, false otherwise.
    pub fn are_members_synced(&self) -> bool {
        self.inner.read().members_synced
    }

    /// Mark this Room as still missing member information.
    pub fn mark_members_missing(&self) {
        self.inner.update_if(|info| {
            // notify observable subscribers only if the previous value was false
            mem::replace(&mut info.members_synced, false)
        })
    }

    /// Check if the room states have been synced
    ///
    /// States might be missing if we have only seen the room_id of this Room
    /// so far, for example as the response for a `create_room` request without
    /// being synced yet.
    ///
    /// Returns true if the state is fully synced, false otherwise.
    pub fn is_state_fully_synced(&self) -> bool {
        self.inner.read().sync_info == SyncInfo::FullySynced
    }

    /// Check if the room state has been at least partially synced.
    ///
    /// See [`Room::is_state_fully_synced`] for more info.
    pub fn is_state_partially_or_fully_synced(&self) -> bool {
        self.inner.read().sync_info != SyncInfo::NoState
    }

    /// Check if the room has its encryption event synced.
    ///
    /// The encryption event can be missing when the room hasn't appeared in
    /// sync yet.
    ///
    /// Returns true if the encryption state is synced, false otherwise.
    pub fn is_encryption_state_synced(&self) -> bool {
        self.inner.read().encryption_state_synced
    }

    /// Get the `prev_batch` token that was received from the last sync. May be
    /// `None` if the last sync contained the full room history.
    pub fn last_prev_batch(&self) -> Option<String> {
        self.inner.read().last_prev_batch.clone()
    }

    /// Get the avatar url of this room.
    pub fn avatar_url(&self) -> Option<OwnedMxcUri> {
        self.inner.read().avatar_url().map(ToOwned::to_owned)
    }

    /// Get information about the avatar of this room.
    pub fn avatar_info(&self) -> Option<avatar::ImageInfo> {
        self.inner.read().avatar_info().map(ToOwned::to_owned)
    }

    /// Get the canonical alias of this room.
    pub fn canonical_alias(&self) -> Option<OwnedRoomAliasId> {
        self.inner.read().canonical_alias().map(ToOwned::to_owned)
    }

    /// Get the canonical alias of this room.
    pub fn alt_aliases(&self) -> Vec<OwnedRoomAliasId> {
        self.inner.read().alt_aliases().to_owned()
    }

    /// Get the `m.room.create` content of this room.
    ///
    /// This usually isn't optional but some servers might not send an
    /// `m.room.create` event as the first event for a given room, thus this can
    /// be optional.
    ///
    /// For room versions earlier than room version 11, if the event is
    /// redacted, all fields except `creator` will be set to their default
    /// value.
    pub fn create_content(&self) -> Option<RoomCreateWithCreatorEventContent> {
        match self.inner.read().base_info.create.as_ref()? {
            MinimalStateEvent::Original(ev) => Some(ev.content.clone()),
            MinimalStateEvent::Redacted(ev) => Some(ev.content.clone()),
        }
    }

    /// Is this room considered a direct message.
    ///
    /// Async because it can read room info from storage.
    #[instrument(skip_all, fields(room_id = ?self.room_id))]
    pub async fn is_direct(&self) -> StoreResult<bool> {
        match self.state() {
            RoomState::Joined | RoomState::Left => {
                Ok(!self.inner.read().base_info.dm_targets.is_empty())
            }

            RoomState::Invited => {
                let member = self.get_member(self.own_user_id()).await?;

                match member {
                    None => {
                        info!("RoomMember not found for the user's own id");
                        Ok(false)
                    }
                    Some(member) => match member.event.as_ref() {
                        MemberEvent::Sync(_) => {
                            warn!("Got MemberEvent::Sync in an invited room");
                            Ok(false)
                        }
                        MemberEvent::Stripped(event) => {
                            Ok(event.content.is_direct.unwrap_or(false))
                        }
                    },
                }
            }

            // TODO: implement logic once we have the stripped events as we'd have with an Invite
            RoomState::Knocked => Ok(false),
        }
    }

    /// If this room is a direct message, get the members that we're sharing the
    /// room with.
    ///
    /// *Note*: The member list might have been modified in the meantime and
    /// the targets might not even be in the room anymore. This setting should
    /// only be considered as guidance. We leave members in this list to allow
    /// us to re-find a DM with a user even if they have left, since we may
    /// want to re-invite them.
    pub fn direct_targets(&self) -> HashSet<OwnedDirectUserIdentifier> {
        self.inner.read().base_info.dm_targets.clone()
    }

    /// If this room is a direct message, returns the number of members that
    /// we're sharing the room with.
    pub fn direct_targets_length(&self) -> usize {
        self.inner.read().base_info.dm_targets.len()
    }

    /// Is the room encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.inner.read().is_encrypted()
    }

    /// Get the `m.room.encryption` content that enabled end to end encryption
    /// in the room.
    pub fn encryption_settings(&self) -> Option<RoomEncryptionEventContent> {
        self.inner.read().base_info.encryption.clone()
    }

    /// Get the guest access policy of this room.
    pub fn guest_access(&self) -> GuestAccess {
        self.inner.read().guest_access().clone()
    }

    /// Get the history visibility policy of this room.
    pub fn history_visibility(&self) -> Option<HistoryVisibility> {
        self.inner.read().history_visibility().cloned()
    }

    /// Get the history visibility policy of this room, or a sensible default if
    /// the event is missing.
    pub fn history_visibility_or_default(&self) -> HistoryVisibility {
        self.inner.read().history_visibility_or_default().clone()
    }

    /// Is the room considered to be public.
    pub fn is_public(&self) -> bool {
        matches!(self.join_rule(), JoinRule::Public)
    }

    /// Get the join rule policy of this room.
    pub fn join_rule(&self) -> JoinRule {
        self.inner.read().join_rule().clone()
    }

    /// Get the maximum power level that this room contains.
    ///
    /// This is useful if one wishes to normalize the power levels, e.g. from
    /// 0-100 where 100 would be the max power level.
    pub fn max_power_level(&self) -> i64 {
        self.inner.read().base_info.max_power_level
    }

    /// Get the current power levels of this room.
    pub async fn power_levels(&self) -> Result<RoomPowerLevels, Error> {
        Ok(self
            .store
            .get_state_event_static::<RoomPowerLevelsEventContent>(self.room_id())
            .await?
            .ok_or(Error::InsufficientData)?
            .deserialize()?
            .power_levels())
    }

    /// Get the `m.room.name` of this room.
    ///
    /// The returned string may be empty if the event has been redacted, or it's
    /// missing from storage.
    pub fn name(&self) -> Option<String> {
        self.inner.read().name().map(ToOwned::to_owned)
    }

    /// Has the room been tombstoned.
    pub fn is_tombstoned(&self) -> bool {
        self.inner.read().base_info.tombstone.is_some()
    }

    /// Get the `m.room.tombstone` content of this room if there is one.
    pub fn tombstone(&self) -> Option<RoomTombstoneEventContent> {
        self.inner.read().tombstone().cloned()
    }

    /// Get the topic of the room.
    pub fn topic(&self) -> Option<String> {
        self.inner.read().topic().map(ToOwned::to_owned)
    }

    /// Is there a non expired membership with application "m.call" and scope
    /// "m.room" in this room
    pub fn has_active_room_call(&self) -> bool {
        self.inner.read().has_active_room_call()
    }

    /// Returns a Vec of userId's that participate in the room call.
    ///
    /// MatrixRTC memberships with application "m.call" and scope "m.room" are
    /// considered. A user can occur twice if they join with two devices.
    /// convert to a set depending if the different users are required or the
    /// amount of sessions.
    ///
    /// The vector is ordered by oldest membership user to newest.
    pub fn active_room_call_participants(&self) -> Vec<OwnedUserId> {
        self.inner.read().active_room_call_participants()
    }

    /// Return the cached display name of the room if it was provided via sync,
    /// or otherwise calculate it, taking into account its name, aliases and
    /// members.
    ///
    /// The display name is calculated according to [this algorithm][spec].
    ///
    /// This is automatically recomputed on every successful sync, and the
    /// cached result can be retrieved in
    /// [`Self::cached_display_name`].
    ///
    /// [spec]: <https://matrix.org/docs/spec/client_server/latest#calculating-the-display-name-for-a-room>
    pub async fn compute_display_name(&self) -> StoreResult<RoomDisplayName> {
        enum DisplayNameOrSummary {
            Summary(RoomSummary),
            DisplayName(RoomDisplayName),
        }

        let display_name_or_summary = {
            let inner = self.inner.read();

            match (inner.name(), inner.canonical_alias()) {
                (Some(name), _) => {
                    let name = RoomDisplayName::Named(name.trim().to_owned());
                    DisplayNameOrSummary::DisplayName(name)
                }
                (None, Some(alias)) => {
                    let name = RoomDisplayName::Aliased(alias.alias().trim().to_owned());
                    DisplayNameOrSummary::DisplayName(name)
                }
                // We can't directly compute the display name from the summary here because Rust
                // thinks that the `inner` lock is still held even if we explicitly call `drop()`
                // on it. So we introduced the DisplayNameOrSummary type and do the computation in
                // two steps.
                (None, None) => DisplayNameOrSummary::Summary(inner.summary.clone()),
            }
        };

        let display_name = match display_name_or_summary {
            DisplayNameOrSummary::Summary(summary) => {
                self.compute_display_name_from_summary(summary).await?
            }
            DisplayNameOrSummary::DisplayName(display_name) => display_name,
        };

        // Update the cached display name before we return the newly computed value.
        self.inner.update_if(|info| {
            if info.cached_display_name.as_ref() != Some(&display_name) {
                info.cached_display_name = Some(display_name.clone());
                true
            } else {
                false
            }
        });

        Ok(display_name)
    }

    /// Compute a [`RoomDisplayName`] from the given [`RoomSummary`].
    async fn compute_display_name_from_summary(
        &self,
        summary: RoomSummary,
    ) -> StoreResult<RoomDisplayName> {
        let summary_member_count = summary.joined_member_count + summary.invited_member_count;

        let (heroes, num_joined_invited_guess) = if !summary.room_heroes.is_empty() {
            let heroes = self.extract_heroes(&summary.room_heroes).await?;
            (heroes, None)
        } else {
            let (heroes, num_joined_invited) = self.compute_summary().await?;
            (heroes, Some(num_joined_invited))
        };

        let num_joined_invited = if self.state() == RoomState::Invited {
            // when we were invited we don't have a proper summary, we have to do best
            // guessing
            heroes.len() as u64 + 1
        } else if summary_member_count == 0 {
            if let Some(num_joined_invited) = num_joined_invited_guess {
                num_joined_invited
            } else {
                self.store
                    .get_user_ids(self.room_id(), RoomMemberships::JOIN | RoomMemberships::INVITE)
                    .await?
                    .len() as u64
            }
        } else {
            summary_member_count
        };

        debug!(
            room_id = ?self.room_id(),
            own_user = ?self.own_user_id,
            num_joined_invited,
            heroes = ?heroes,
            "Calculating name for a room based on heroes",
        );

        let display_name = compute_display_name_from_heroes(
            num_joined_invited,
            heroes.iter().map(|hero| hero.as_str()).collect(),
        );

        Ok(display_name)
    }

    /// Extract and collect the display names of the room heroes from a
    /// [`RoomSummary`].
    ///
    /// Returns the display names as a list of strings.
    async fn extract_heroes(&self, heroes: &[RoomHero]) -> StoreResult<Vec<String>> {
        let own_user_id = self.own_user_id().as_str();

        let mut names = Vec::with_capacity(heroes.len());
        let heroes = heroes.iter().filter(|hero| hero.user_id != own_user_id);

        for hero in heroes {
            if let Some(display_name) = &hero.display_name {
                names.push(display_name.clone());
            } else {
                match self.get_member(&hero.user_id).await {
                    Ok(Some(member)) => {
                        names.push(member.name().to_owned());
                    }
                    Ok(None) => {
                        warn!("Ignoring hero, no member info for {}", hero.user_id);
                    }
                    Err(error) => {
                        warn!("Ignoring hero, error getting member: {}", error);
                    }
                }
            }
        }

        Ok(names)
    }

    /// Compute the room summary with the data present in the store.
    ///
    /// The summary might be incorrect if the database info is outdated.
    ///
    /// Returns a `(heroes_names, num_joined_invited)` tuple.
    async fn compute_summary(&self) -> StoreResult<(Vec<String>, u64)> {
        let mut members = self.members(RoomMemberships::JOIN | RoomMemberships::INVITE).await?;

        // We can make a good prediction of the total number of joined and invited
        // members here. This might be incorrect if the database info is
        // outdated.
        let num_joined_invited = members.len() as u64;

        if num_joined_invited == 0
            || (num_joined_invited == 1 && members[0].user_id() == self.own_user_id)
        {
            // No joined or invited members, heroes should be banned and left members.
            members = self.members(RoomMemberships::LEAVE | RoomMemberships::BAN).await?;
        }

        // Make the ordering deterministic.
        members.sort_unstable_by(|lhs, rhs| lhs.name().cmp(rhs.name()));

        let heroes = members
            .into_iter()
            .filter(|u| u.user_id() != self.own_user_id)
            .take(NUM_HEROES)
            .map(|u| u.name().to_owned())
            .collect();

        Ok((heroes, num_joined_invited))
    }

    /// Returns the cached computed display name, if available.
    ///
    /// This cache is refilled every time we call
    /// [`Self::compute_display_name`].
    pub fn cached_display_name(&self) -> Option<RoomDisplayName> {
        self.inner.read().cached_display_name.clone()
    }

    /// Update the cached user defined notification mode.
    ///
    /// This is automatically recomputed on every successful sync, and the
    /// cached result can be retrieved in
    /// [`Self::cached_user_defined_notification_mode`].
    pub fn update_cached_user_defined_notification_mode(&self, mode: RoomNotificationMode) {
        self.inner.update_if(|info| {
            if info.cached_user_defined_notification_mode.as_ref() != Some(&mode) {
                info.cached_user_defined_notification_mode = Some(mode);

                true
            } else {
                false
            }
        });
    }

    /// Returns the cached user defined notification mode, if available.
    ///
    /// This cache is refilled every time we call
    /// [`Self::update_cached_user_defined_notification_mode`].
    pub fn cached_user_defined_notification_mode(&self) -> Option<RoomNotificationMode> {
        self.inner.read().cached_user_defined_notification_mode
    }

    /// Return the last event in this room, if one has been cached during
    /// sliding sync.
    #[cfg(feature = "experimental-sliding-sync")]
    pub fn latest_event(&self) -> Option<LatestEvent> {
        self.inner.read().latest_event.as_deref().cloned()
    }

    /// Return the most recent few encrypted events. When the keys come through
    /// to decrypt these, the most recent relevant one will replace
    /// latest_event. (We can't tell which one is relevant until
    /// they are decrypted.)
    #[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
    pub(crate) fn latest_encrypted_events(&self) -> Vec<Raw<AnySyncTimelineEvent>> {
        self.latest_encrypted_events.read().unwrap().iter().cloned().collect()
    }

    /// Replace our latest_event with the supplied event, and delete it and all
    /// older encrypted events from latest_encrypted_events, given that the
    /// new event was at the supplied index in the latest_encrypted_events
    /// list.
    ///
    /// Panics if index is not a valid index in the latest_encrypted_events
    /// list.
    ///
    /// It is the responsibility of the caller to apply the changes into the
    /// state store after calling this function.
    #[cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]
    pub(crate) fn on_latest_event_decrypted(
        &self,
        latest_event: Box<LatestEvent>,
        index: usize,
        changes: &mut crate::StateChanges,
        room_info_notable_updates: &mut BTreeMap<OwnedRoomId, RoomInfoNotableUpdateReasons>,
    ) {
        self.latest_encrypted_events.write().unwrap().drain(0..=index);

        let room_info = changes
            .room_infos
            .entry(self.room_id().to_owned())
            .or_insert_with(|| self.clone_info());

        room_info.latest_event = Some(latest_event);

        room_info_notable_updates
            .entry(self.room_id().to_owned())
            .or_default()
            .insert(RoomInfoNotableUpdateReasons::LATEST_EVENT);
    }

    /// Get the list of users ids that are considered to be joined members of
    /// this room.
    pub async fn joined_user_ids(&self) -> StoreResult<Vec<OwnedUserId>> {
        self.store.get_user_ids(self.room_id(), RoomMemberships::JOIN).await
    }

    /// Get the `RoomMember`s of this room that are known to the store, with the
    /// given memberships.
    pub async fn members(&self, memberships: RoomMemberships) -> StoreResult<Vec<RoomMember>> {
        let user_ids = self.store.get_user_ids(self.room_id(), memberships).await?;

        if user_ids.is_empty() {
            return Ok(Vec::new());
        }

        let member_events = self
            .store
            .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(
                self.room_id(),
                &user_ids,
            )
            .await?
            .into_iter()
            .map(|raw_event| raw_event.deserialize())
            .collect::<Result<Vec<_>, _>>()?;

        let mut profiles = self.store.get_profiles(self.room_id(), &user_ids).await?;

        let mut presences = self
            .store
            .get_presence_events(&user_ids)
            .await?
            .into_iter()
            .filter_map(|e| {
                e.deserialize().ok().map(|presence| (presence.sender.clone(), presence))
            })
            .collect::<BTreeMap<_, _>>();

        let display_names = member_events.iter().map(|e| e.display_name()).collect::<Vec<_>>();
        let room_info = self.member_room_info(&display_names).await?;

        let mut members = Vec::new();

        for event in member_events {
            let profile = profiles.remove(event.user_id());
            let presence = presences.remove(event.user_id());
            members.push(RoomMember::from_parts(event, profile, presence, &room_info))
        }

        Ok(members)
    }

    /// Get the heroes for this room.
    pub fn heroes(&self) -> Vec<RoomHero> {
        self.inner.read().heroes().to_vec()
    }

    /// Returns the number of members who have joined or been invited to the
    /// room.
    pub fn active_members_count(&self) -> u64 {
        self.inner.read().active_members_count()
    }

    /// Returns the number of members who have been invited to the room.
    pub fn invited_members_count(&self) -> u64 {
        self.inner.read().invited_members_count()
    }

    /// Returns the number of members who have joined the room.
    pub fn joined_members_count(&self) -> u64 {
        self.inner.read().joined_members_count()
    }

    /// Subscribe to the inner `RoomInfo`.
    pub fn subscribe_info(&self) -> Subscriber<RoomInfo> {
        self.inner.subscribe()
    }

    /// Clone the inner `RoomInfo`.
    pub fn clone_info(&self) -> RoomInfo {
        self.inner.get()
    }

    /// Update the summary with given RoomInfo.
    pub fn set_room_info(
        &self,
        room_info: RoomInfo,
        room_info_notable_update_reasons: RoomInfoNotableUpdateReasons,
    ) {
        self.inner.set(room_info);

        // Ignore error if no receiver exists.
        let _ = self.room_info_notable_update_sender.send(RoomInfoNotableUpdate {
            room_id: self.room_id.clone(),
            reasons: room_info_notable_update_reasons,
        });
    }

    /// Get the `RoomMember` with the given `user_id`.
    ///
    /// Returns `None` if the member was never part of this room, otherwise
    /// return a `RoomMember` that can be in a joined, RoomState::Invited, left,
    /// banned state.
    ///
    /// Async because it can read from storage.
    pub async fn get_member(&self, user_id: &UserId) -> StoreResult<Option<RoomMember>> {
        let Some(raw_event) = self.store.get_member_event(self.room_id(), user_id).await? else {
            debug!(%user_id, "Member event not found in state store");
            return Ok(None);
        };

        let event = raw_event.deserialize()?;

        let presence =
            self.store.get_presence_event(user_id).await?.and_then(|e| e.deserialize().ok());

        let profile = self.store.get_profile(self.room_id(), user_id).await?;

        let display_names = [event.display_name()];
        let room_info = self.member_room_info(&display_names).await?;

        Ok(Some(RoomMember::from_parts(event, profile, presence, &room_info)))
    }

    /// The current `MemberRoomInfo` for this room.
    ///
    /// Async because it can read from storage.
    async fn member_room_info<'a>(
        &self,
        display_names: &'a [DisplayName],
    ) -> StoreResult<MemberRoomInfo<'a>> {
        let max_power_level = self.max_power_level();
        let room_creator = self.inner.read().creator().map(ToOwned::to_owned);

        let power_levels = self
            .store
            .get_state_event_static(self.room_id())
            .await?
            .and_then(|e| e.deserialize().ok());

        let users_display_names =
            self.store.get_users_with_display_names(self.room_id(), display_names).await?;

        let ignored_users = self
            .store
            .get_account_data_event_static::<IgnoredUserListEventContent>()
            .await?
            .map(|c| c.deserialize())
            .transpose()?
            .map(|e| e.content.ignored_users.into_keys().collect());

        Ok(MemberRoomInfo {
            power_levels: power_levels.into(),
            max_power_level,
            room_creator,
            users_display_names,
            ignored_users,
        })
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

    /// Check whether the room is marked as favourite.
    ///
    /// A room is considered favourite if it has received the `m.favourite` tag.
    pub fn is_favourite(&self) -> bool {
        self.inner.read().base_info.notable_tags.contains(RoomNotableTags::FAVOURITE)
    }

    /// Check whether the room is marked as low priority.
    ///
    /// A room is considered low priority if it has received the `m.lowpriority`
    /// tag.
    pub fn is_low_priority(&self) -> bool {
        self.inner.read().base_info.notable_tags.contains(RoomNotableTags::LOW_PRIORITY)
    }

    /// Get the receipt as an `OwnedEventId` and `Receipt` tuple for the given
    /// `receipt_type`, `thread` and `user_id` in this room.
    pub async fn load_user_receipt(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> StoreResult<Option<(OwnedEventId, Receipt)>> {
        self.store.get_user_room_receipt_event(self.room_id(), receipt_type, thread, user_id).await
    }

    /// Load from storage the receipts as a list of `OwnedUserId` and `Receipt`
    /// tuples for the given `receipt_type`, `thread` and `event_id` in this
    /// room.
    pub async fn load_event_receipts(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> StoreResult<Vec<(OwnedUserId, Receipt)>> {
        self.store
            .get_event_room_receipt_events(self.room_id(), receipt_type, thread, event_id)
            .await
    }

    /// Returns a boolean indicating if this room has been manually marked as
    /// unread
    pub fn is_marked_unread(&self) -> bool {
        self.inner.read().base_info.is_marked_unread
    }

    /// Returns the recency stamp of the room.
    ///
    /// Please read `RoomInfo::recency_stamp` to learn more.
    #[cfg(feature = "experimental-sliding-sync")]
    pub fn recency_stamp(&self) -> Option<u64> {
        self.inner.read().recency_stamp
    }

    /// Get a `Stream` of loaded pinned events for this room.
    /// If no pinned events are found a single empty `Vec` will be returned.
    pub fn pinned_event_ids_stream(&self) -> impl Stream<Item = Vec<OwnedEventId>> {
        self.inner
            .subscribe()
            .map(|i| i.base_info.pinned_events.map(|c| c.pinned).unwrap_or_default())
    }

    /// Returns the current pinned event ids for this room.
    pub fn pinned_event_ids(&self) -> Option<Vec<OwnedEventId>> {
        self.inner.read().pinned_event_ids()
    }
}

// See https://github.com/matrix-org/matrix-rust-sdk/pull/3749#issuecomment-2312939823.
#[cfg(not(feature = "test-send-sync"))]
unsafe impl Send for Room {}

// See https://github.com/matrix-org/matrix-rust-sdk/pull/3749#issuecomment-2312939823.
#[cfg(not(feature = "test-send-sync"))]
unsafe impl Sync for Room {}

#[cfg(feature = "test-send-sync")]
#[test]
// See https://github.com/matrix-org/matrix-rust-sdk/pull/3749#issuecomment-2312939823.
fn test_send_sync_for_room() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<Room>();
}

/// The underlying pure data structure for joined and left rooms.
///
/// Holds all the info needed to persist a room into the state store.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomInfo {
    /// The version of the room info.
    #[serde(default)]
    pub(crate) version: u8,

    /// The unique room id of the room.
    pub(crate) room_id: OwnedRoomId,

    /// The state of the room.
    pub(crate) room_state: RoomState,

    /// The previous state of the room, if any.
    pub(crate) prev_room_state: Option<RoomState>,

    /// The unread notifications counts, as returned by the server.
    ///
    /// These might be incorrect for encrypted rooms, since the server doesn't
    /// have access to the content of the encrypted events.
    pub(crate) notification_counts: UnreadNotificationsCount,

    /// The summary of this room.
    pub(crate) summary: RoomSummary,

    /// Flag remembering if the room members are synced.
    pub(crate) members_synced: bool,

    /// The prev batch of this room we received during the last sync.
    pub(crate) last_prev_batch: Option<String>,

    /// How much we know about this room.
    pub(crate) sync_info: SyncInfo,

    /// Whether or not the encryption info was been synced.
    pub(crate) encryption_state_synced: bool,

    /// The last event send by sliding sync
    #[cfg(feature = "experimental-sliding-sync")]
    pub(crate) latest_event: Option<Box<LatestEvent>>,

    /// Information about read receipts for this room.
    #[serde(default)]
    pub(crate) read_receipts: RoomReadReceipts,

    /// Base room info which holds some basic event contents important for the
    /// room state.
    pub(crate) base_info: Box<BaseRoomInfo>,

    /// Did we already warn about an unknown room version in
    /// [`RoomInfo::room_version_or_default`]? This is done to avoid
    /// spamming about unknown room versions in the log for the same room.
    #[serde(skip)]
    pub(crate) warned_about_unknown_room_version: Arc<AtomicBool>,

    /// Cached display name, useful for sync access.
    ///
    /// Filled by calling [`Room::compute_display_name`]. It's automatically
    /// filled at start when creating a room, or on every successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cached_display_name: Option<RoomDisplayName>,

    /// Cached user defined notification mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cached_user_defined_notification_mode: Option<RoomNotificationMode>,

    /// The recency stamp of this room.
    ///
    /// It's not to be confused with `origin_server_ts` of the latest event.
    /// Sliding Sync might "ignore” some events when computing the recency
    /// stamp of the room. Thus, using this `recency_stamp` value is
    /// more accurate than relying on the latest event.
    #[cfg(feature = "experimental-sliding-sync")]
    #[serde(default)]
    pub(crate) recency_stamp: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum SyncInfo {
    /// We only know the room exists and whether it is in invite / joined / left
    /// state.
    ///
    /// This is the case when we have a limited sync or only seen the room
    /// because of a request we've done, like a room creation event.
    NoState,

    /// Some states have been synced, but they might have been filtered or is
    /// stale, as it is from a room we've left.
    PartiallySynced,

    /// We have all the latest state events.
    FullySynced,
}

impl RoomInfo {
    #[doc(hidden)] // used by store tests, otherwise it would be pub(crate)
    pub fn new(room_id: &RoomId, room_state: RoomState) -> Self {
        Self {
            version: 1,
            room_id: room_id.into(),
            room_state,
            prev_room_state: None,
            notification_counts: Default::default(),
            summary: Default::default(),
            members_synced: false,
            last_prev_batch: None,
            sync_info: SyncInfo::NoState,
            encryption_state_synced: false,
            #[cfg(feature = "experimental-sliding-sync")]
            latest_event: None,
            read_receipts: Default::default(),
            base_info: Box::new(BaseRoomInfo::new()),
            warned_about_unknown_room_version: Arc::new(false.into()),
            cached_display_name: None,
            cached_user_defined_notification_mode: None,
            #[cfg(feature = "experimental-sliding-sync")]
            recency_stamp: None,
        }
    }

    /// Mark this Room as joined.
    pub fn mark_as_joined(&mut self) {
        self.set_state(RoomState::Joined);
    }

    /// Mark this Room as left.
    pub fn mark_as_left(&mut self) {
        self.set_state(RoomState::Left);
    }

    /// Mark this Room as invited.
    pub fn mark_as_invited(&mut self) {
        self.set_state(RoomState::Invited);
    }

    /// Mark this Room as knocked.
    pub fn mark_as_knocked(&mut self) {
        self.set_state(RoomState::Knocked);
    }

    /// Set the membership RoomState of this Room
    pub fn set_state(&mut self, room_state: RoomState) {
        if room_state != self.room_state {
            self.prev_room_state = Some(self.room_state);
            self.room_state = room_state;
        }
    }

    /// Mark this Room as having all the members synced.
    pub fn mark_members_synced(&mut self) {
        self.members_synced = true;
    }

    /// Mark this Room as still missing member information.
    pub fn mark_members_missing(&mut self) {
        self.members_synced = false;
    }

    /// Mark this Room as still missing some state information.
    pub fn mark_state_partially_synced(&mut self) {
        self.sync_info = SyncInfo::PartiallySynced;
    }

    /// Mark this Room as still having all state synced.
    pub fn mark_state_fully_synced(&mut self) {
        self.sync_info = SyncInfo::FullySynced;
    }

    /// Mark this Room as still having no state synced.
    pub fn mark_state_not_synced(&mut self) {
        self.sync_info = SyncInfo::NoState;
    }

    /// Mark this Room as having the encryption state synced.
    pub fn mark_encryption_state_synced(&mut self) {
        self.encryption_state_synced = true;
    }

    /// Mark this Room as still missing encryption state information.
    pub fn mark_encryption_state_missing(&mut self) {
        self.encryption_state_synced = false;
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

    /// Returns the state this room is in.
    pub fn state(&self) -> RoomState {
        self.room_state
    }

    /// Returns whether this is an encrypted room.
    pub fn is_encrypted(&self) -> bool {
        self.base_info.encryption.is_some()
    }

    /// Set the encryption event content in this room.
    pub fn set_encryption_event(&mut self, event: Option<RoomEncryptionEventContent>) {
        self.base_info.encryption = event;
    }

    /// Handle the given state event.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_state_event(&mut self, event: &AnySyncStateEvent) -> bool {
        let ret = self.base_info.handle_state_event(event);

        // If we received an `m.room.encryption` event here, and encryption got enabled,
        // then we can be certain that we have synced the encryption state event, so
        // mark it here as synced.
        if let AnySyncStateEvent::RoomEncryption(_) = event {
            if self.is_encrypted() {
                self.mark_encryption_state_synced();
            }
        }

        ret
    }

    /// Handle the given stripped state event.
    ///
    /// Returns true if the event modified the info, false otherwise.
    pub fn handle_stripped_state_event(&mut self, event: &AnyStrippedStateEvent) -> bool {
        self.base_info.handle_stripped_state_event(event)
    }

    /// Handle the given redaction.
    #[instrument(skip_all, fields(redacts))]
    pub fn handle_redaction(
        &mut self,
        event: &SyncRoomRedactionEvent,
        _raw: &Raw<SyncRoomRedactionEvent>,
    ) {
        let room_version = self.base_info.room_version().unwrap_or(&RoomVersionId::V1);

        let Some(redacts) = event.redacts(room_version) else {
            info!("Can't apply redaction, redacts field is missing");
            return;
        };
        tracing::Span::current().record("redacts", debug(redacts));

        #[cfg(feature = "experimental-sliding-sync")]
        if let Some(latest_event) = &mut self.latest_event {
            tracing::trace!("Checking if redaction applies to latest event");
            if latest_event.event_id().as_deref() == Some(redacts) {
                match apply_redaction(latest_event.event().raw(), _raw, room_version) {
                    Some(redacted) => {
                        // Even if the original event was encrypted, redaction removes all its
                        // fields so it cannot possibly be successfully decrypted after redaction.
                        latest_event.event_mut().kind =
                            TimelineEventKind::PlainText { event: redacted };
                        debug!("Redacted latest event");
                    }
                    None => {
                        self.latest_event = None;
                        debug!("Removed latest event");
                    }
                }
            }
        }

        self.base_info.handle_redaction(redacts);
    }

    /// Returns the current room avatar.
    pub fn avatar_url(&self) -> Option<&MxcUri> {
        self.base_info
            .avatar
            .as_ref()
            .and_then(|e| e.as_original().and_then(|e| e.content.url.as_deref()))
    }

    /// Update the room avatar.
    pub fn update_avatar(&mut self, url: Option<OwnedMxcUri>) {
        self.base_info.avatar = url.map(|url| {
            let mut content = RoomAvatarEventContent::new();
            content.url = Some(url);

            MinimalStateEvent::Original(OriginalMinimalStateEvent { content, event_id: None })
        });
    }

    /// Returns information about the current room avatar.
    pub fn avatar_info(&self) -> Option<&avatar::ImageInfo> {
        self.base_info
            .avatar
            .as_ref()
            .and_then(|e| e.as_original().and_then(|e| e.content.info.as_deref()))
    }

    /// Update the notifications count.
    pub fn update_notification_count(&mut self, notification_counts: UnreadNotificationsCount) {
        self.notification_counts = notification_counts;
    }

    /// Update the RoomSummary from a Ruma `RoomSummary`.
    ///
    /// Returns true if any field has been updated, false otherwise.
    pub fn update_from_ruma_summary(&mut self, summary: &RumaSummary) -> bool {
        let mut changed = false;

        if !summary.is_empty() {
            if !summary.heroes.is_empty() {
                self.summary.room_heroes = summary
                    .heroes
                    .iter()
                    .map(|hero_id| RoomHero {
                        user_id: hero_id.to_owned(),
                        display_name: None,
                        avatar_url: None,
                    })
                    .collect();

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

    /// Updates the joined member count.
    #[cfg(feature = "experimental-sliding-sync")]
    pub(crate) fn update_joined_member_count(&mut self, count: u64) {
        self.summary.joined_member_count = count;
    }

    /// Updates the invited member count.
    #[cfg(feature = "experimental-sliding-sync")]
    pub(crate) fn update_invited_member_count(&mut self, count: u64) {
        self.summary.invited_member_count = count;
    }

    /// Updates the room heroes.
    #[cfg(feature = "experimental-sliding-sync")]
    pub(crate) fn update_heroes(&mut self, heroes: Vec<RoomHero>) {
        self.summary.room_heroes = heroes;
    }

    /// The heroes for this room.
    pub fn heroes(&self) -> &[RoomHero] {
        &self.summary.room_heroes
    }

    /// The number of active members (invited + joined) in the room.
    ///
    /// The return value is saturated at `u64::MAX`.
    pub fn active_members_count(&self) -> u64 {
        self.summary.joined_member_count.saturating_add(self.summary.invited_member_count)
    }

    /// The number of invited members in the room
    pub fn invited_members_count(&self) -> u64 {
        self.summary.invited_member_count
    }

    /// The number of joined members in the room
    pub fn joined_members_count(&self) -> u64 {
        self.summary.joined_member_count
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
        self.base_info.room_version()
    }

    /// Get the room version of this room, or a sensible default.
    ///
    /// Will warn (at most once) if the room creation event is missing from this
    /// [`RoomInfo`].
    pub fn room_version_or_default(&self) -> RoomVersionId {
        use std::sync::atomic::Ordering;

        self.base_info.room_version().cloned().unwrap_or_else(|| {
            if self
                .warned_about_unknown_room_version
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                warn!("Unknown room version, falling back to v10");
            }

            RoomVersionId::V10
        })
    }

    /// Get the room type of this room.
    pub fn room_type(&self) -> Option<&RoomType> {
        match self.base_info.create.as_ref()? {
            MinimalStateEvent::Original(ev) => ev.content.room_type.as_ref(),
            MinimalStateEvent::Redacted(ev) => ev.content.room_type.as_ref(),
        }
    }

    /// Get the creator of this room.
    pub fn creator(&self) -> Option<&UserId> {
        match self.base_info.create.as_ref()? {
            MinimalStateEvent::Original(ev) => Some(&ev.content.creator),
            MinimalStateEvent::Redacted(ev) => Some(&ev.content.creator),
        }
    }

    fn guest_access(&self) -> &GuestAccess {
        match &self.base_info.guest_access {
            Some(MinimalStateEvent::Original(ev)) => &ev.content.guest_access,
            _ => &GuestAccess::Forbidden,
        }
    }

    /// Returns the history visibility for this room.
    ///
    /// Returns None if the event was never seen during sync.
    pub fn history_visibility(&self) -> Option<&HistoryVisibility> {
        match &self.base_info.history_visibility {
            Some(MinimalStateEvent::Original(ev)) => Some(&ev.content.history_visibility),
            _ => None,
        }
    }

    /// Returns the history visibility for this room, or a sensible default.
    ///
    /// Returns `Shared`, the default specified by the [spec], when the event is
    /// missing.
    ///
    /// [spec]: https://spec.matrix.org/latest/client-server-api/#server-behaviour-7
    pub fn history_visibility_or_default(&self) -> &HistoryVisibility {
        match &self.base_info.history_visibility {
            Some(MinimalStateEvent::Original(ev)) => &ev.content.history_visibility,
            _ => &HistoryVisibility::Shared,
        }
    }

    /// Returns the join rule for this room.
    ///
    /// Defaults to `Public`, if missing.
    pub fn join_rule(&self) -> &JoinRule {
        match &self.base_info.join_rules {
            Some(MinimalStateEvent::Original(ev)) => &ev.content.join_rule,
            _ => &JoinRule::Public,
        }
    }

    /// Get the name of this room.
    pub fn name(&self) -> Option<&str> {
        let name = &self.base_info.name.as_ref()?.as_original()?.content.name;
        (!name.is_empty()).then_some(name)
    }

    fn tombstone(&self) -> Option<&RoomTombstoneEventContent> {
        Some(&self.base_info.tombstone.as_ref()?.as_original()?.content)
    }

    /// Returns the topic for this room, if set.
    pub fn topic(&self) -> Option<&str> {
        Some(&self.base_info.topic.as_ref()?.as_original()?.content.topic)
    }

    /// Get a list of all the valid (non expired) matrixRTC memberships and
    /// associated UserId's in this room.
    ///
    /// The vector is ordered by oldest membership to newest.
    fn active_matrix_rtc_memberships(&self) -> Vec<(CallMemberStateKey, MembershipData<'_>)> {
        let mut v = self
            .base_info
            .rtc_member_events
            .iter()
            .filter_map(|(user_id, ev)| {
                ev.as_original().map(|ev| {
                    ev.content
                        .active_memberships(None)
                        .into_iter()
                        .map(move |m| (user_id.clone(), m))
                })
            })
            .flatten()
            .collect::<Vec<_>>();
        v.sort_by_key(|(_, m)| m.created_ts());
        v
    }

    /// Similar to
    /// [`matrix_rtc_memberships`](Self::active_matrix_rtc_memberships) but only
    /// returns Memberships with application "m.call" and scope "m.room".
    ///
    /// The vector is ordered by oldest membership user to newest.
    fn active_room_call_memberships(&self) -> Vec<(CallMemberStateKey, MembershipData<'_>)> {
        self.active_matrix_rtc_memberships()
            .into_iter()
            .filter(|(_user_id, m)| m.is_room_call())
            .collect()
    }

    /// Is there a non expired membership with application "m.call" and scope
    /// "m.room" in this room.
    pub fn has_active_room_call(&self) -> bool {
        !self.active_room_call_memberships().is_empty()
    }

    /// Returns a Vec of userId's that participate in the room call.
    ///
    /// matrix_rtc memberships with application "m.call" and scope "m.room" are
    /// considered. A user can occur twice if they join with two devices.
    /// convert to a set depending if the different users are required or the
    /// amount of sessions.
    ///
    /// The vector is ordered by oldest membership user to newest.
    pub fn active_room_call_participants(&self) -> Vec<OwnedUserId> {
        self.active_room_call_memberships()
            .iter()
            .map(|(call_member_state_key, _)| call_member_state_key.user_id().to_owned())
            .collect()
    }

    /// Returns the latest (decrypted) event recorded for this room.
    #[cfg(feature = "experimental-sliding-sync")]
    pub fn latest_event(&self) -> Option<&LatestEvent> {
        self.latest_event.as_deref()
    }

    /// Updates the recency stamp of this room.
    ///
    /// Please read [`Self::recency_stamp`] to learn more.
    #[cfg(feature = "experimental-sliding-sync")]
    pub(crate) fn update_recency_stamp(&mut self, stamp: u64) {
        self.recency_stamp = Some(stamp);
    }

    /// Returns the current pinned event ids for this room.
    pub fn pinned_event_ids(&self) -> Option<Vec<OwnedEventId>> {
        self.base_info.pinned_events.clone().map(|c| c.pinned)
    }

    /// Checks if an `EventId` is currently pinned.
    /// It avoids having to clone the whole list of event ids to check a single
    /// value.
    ///
    /// Returns `true` if the provided `event_id` is pinned, `false` otherwise.
    pub fn is_pinned_event(&self, event_id: &EventId) -> bool {
        self.base_info
            .pinned_events
            .as_ref()
            .map(|p| p.pinned.contains(&event_id.to_owned()))
            .unwrap_or_default()
    }

    /// Apply migrations to this `RoomInfo` if needed.
    ///
    /// This should be used to populate new fields with data from the state
    /// store.
    ///
    /// Returns `true` if migrations were applied and this `RoomInfo` needs to
    /// be persisted to the state store.
    #[instrument(skip_all, fields(room_id = ?self.room_id))]
    pub(crate) async fn apply_migrations(&mut self, store: Arc<DynStateStore>) -> bool {
        let mut migrated = false;

        if self.version < 1 {
            info!("Migrating room info to version 1");

            // notable_tags
            match store.get_room_account_data_event_static::<TagEventContent>(&self.room_id).await {
                // Pinned events are never in stripped state.
                Ok(Some(raw_event)) => match raw_event.deserialize() {
                    Ok(event) => {
                        self.base_info.handle_notable_tags(&event.content.tags);
                    }
                    Err(error) => {
                        warn!("Failed to deserialize room tags: {error}");
                    }
                },
                Ok(_) => {
                    // Nothing to do.
                }
                Err(error) => {
                    warn!("Failed to load room tags: {error}");
                }
            }

            // pinned_events
            match store.get_state_event_static::<RoomPinnedEventsEventContent>(&self.room_id).await
            {
                // Pinned events are never in stripped state.
                Ok(Some(RawSyncOrStrippedState::Sync(raw_event))) => {
                    match raw_event.deserialize() {
                        Ok(event) => {
                            self.handle_state_event(&event.into());
                        }
                        Err(error) => {
                            warn!("Failed to deserialize room pinned events: {error}");
                        }
                    }
                }
                Ok(_) => {
                    // Nothing to do.
                }
                Err(error) => {
                    warn!("Failed to load room pinned events: {error}");
                }
            }

            self.version = 1;
            migrated = true;
        }

        migrated
    }
}

#[cfg(feature = "experimental-sliding-sync")]
fn apply_redaction(
    event: &Raw<AnySyncTimelineEvent>,
    raw_redaction: &Raw<SyncRoomRedactionEvent>,
    room_version: &RoomVersionId,
) -> Option<Raw<AnySyncTimelineEvent>> {
    use ruma::canonical_json::{redact_in_place, RedactedBecause};

    let mut event_json = match event.deserialize_as() {
        Ok(json) => json,
        Err(e) => {
            warn!("Failed to deserialize latest event: {e}");
            return None;
        }
    };

    let redacted_because = match RedactedBecause::from_raw_event(raw_redaction) {
        Ok(rb) => rb,
        Err(e) => {
            warn!("Redaction event is not valid canonical JSON: {e}");
            return None;
        }
    };

    let redact_result = redact_in_place(&mut event_json, room_version, Some(redacted_because));

    if let Err(e) = redact_result {
        warn!("Failed to redact latest event: {e}");
        return None;
    }

    let raw = Raw::new(&event_json).expect("CanonicalJsonObject must be serializable");
    Some(raw.cast())
}

bitflags! {
    /// Room state filter as a bitset.
    ///
    /// Note that [`RoomStateFilter::empty()`] doesn't filter the results and
    /// is equivalent to [`RoomStateFilter::all()`].
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct RoomStateFilter: u16 {
        /// The room is in a joined state.
        const JOINED   = 0b00000001;
        /// The room is in an invited state.
        const INVITED  = 0b00000010;
        /// The room is in a left state.
        const LEFT     = 0b00000100;
        /// The room is in a knocked state.
        const KNOCKED  = 0b00001000;
    }
}

impl RoomStateFilter {
    /// Whether the given room state matches this `RoomStateFilter`.
    pub fn matches(&self, state: RoomState) -> bool {
        if self.is_empty() {
            return true;
        }

        let bit_state = match state {
            RoomState::Joined => Self::JOINED,
            RoomState::Left => Self::LEFT,
            RoomState::Invited => Self::INVITED,
            RoomState::Knocked => Self::KNOCKED,
        };

        self.contains(bit_state)
    }

    /// Get this `RoomStateFilter` as a list of matching [`RoomState`]s.
    pub fn as_vec(&self) -> Vec<RoomState> {
        let mut states = Vec::new();

        if self.contains(Self::JOINED) {
            states.push(RoomState::Joined);
        }
        if self.contains(Self::LEFT) {
            states.push(RoomState::Left);
        }
        if self.contains(Self::INVITED) {
            states.push(RoomState::Invited);
        }

        states
    }
}

/// Calculate room name according to step 3 of the [naming algorithm].
///
/// [naming algorithm]: https://spec.matrix.org/latest/client-server-api/#calculating-the-display-name-for-a-room
fn compute_display_name_from_heroes(
    num_joined_invited: u64,
    mut heroes: Vec<&str>,
) -> RoomDisplayName {
    let num_heroes = heroes.len() as u64;
    let num_joined_invited_except_self = num_joined_invited.saturating_sub(1);

    // Stabilize ordering.
    heroes.sort_unstable();

    let names = if num_heroes == 0 && num_joined_invited > 1 {
        format!("{} people", num_joined_invited)
    } else if num_heroes >= num_joined_invited_except_self {
        heroes.join(", ")
    } else if num_heroes < num_joined_invited_except_self && num_joined_invited > 1 {
        // TODO: What length does the spec want us to use here and in
        // the `else`?
        format!("{}, and {} others", heroes.join(", "), (num_joined_invited - num_heroes))
    } else {
        "".to_owned()
    };

    // User is alone.
    if num_joined_invited <= 1 {
        if names.is_empty() {
            RoomDisplayName::Empty
        } else {
            RoomDisplayName::EmptyWas(names)
        }
    } else {
        RoomDisplayName::Calculated(names)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ops::{Not, Sub},
        str::FromStr,
        sync::Arc,
        time::Duration,
    };

    use assign::assign;
    #[cfg(feature = "experimental-sliding-sync")]
    use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
    use matrix_sdk_test::{
        async_test,
        event_factory::EventFactory,
        test_json::{sync_events::PINNED_EVENTS, TAG},
        ALICE, BOB, CAROL,
    };
    use ruma::{
        api::client::sync::sync_events::v3::RoomSummary as RumaSummary,
        device_id, event_id,
        events::{
            call::member::{
                ActiveFocus, ActiveLivekitFocus, Application, CallApplicationContent,
                CallMemberEventContent, CallMemberStateKey, Focus, LegacyMembershipData,
                LegacyMembershipDataInit, LivekitFocus, OriginalSyncCallMemberEvent,
            },
            room::{
                canonical_alias::RoomCanonicalAliasEventContent,
                encryption::{OriginalSyncRoomEncryptionEvent, RoomEncryptionEventContent},
                member::{MembershipState, RoomMemberEventContent, StrippedRoomMemberEvent},
                name::RoomNameEventContent,
                pinned_events::RoomPinnedEventsEventContent,
            },
            AnySyncStateEvent, EmptyStateKey, StateEventType, StateUnsigned, SyncStateEvent,
        },
        owned_event_id, owned_user_id, room_alias_id, room_id,
        serde::Raw,
        time::SystemTime,
        user_id, DeviceId, EventEncryptionAlgorithm, EventId, MilliSecondsSinceUnixEpoch,
        OwnedEventId, OwnedUserId, UserId,
    };
    use serde_json::json;
    use stream_assert::{assert_pending, assert_ready};

    use super::{compute_display_name_from_heroes, Room, RoomHero, RoomInfo, RoomState, SyncInfo};
    #[cfg(any(feature = "experimental-sliding-sync", feature = "e2e-encryption"))]
    use crate::latest_event::LatestEvent;
    use crate::{
        rooms::RoomNotableTags,
        store::{IntoStateStore, MemoryStore, StateChanges, StateStore, StoreConfig},
        BaseClient, MinimalStateEvent, OriginalMinimalStateEvent, RoomDisplayName,
        RoomInfoNotableUpdateReasons, SessionMeta,
    };

    #[test]
    #[cfg(feature = "experimental-sliding-sync")]
    fn test_room_info_serialization() {
        // This test exists to make sure we don't accidentally change the
        // serialized format for `RoomInfo`.

        use ruma::owned_user_id;

        use super::RoomSummary;
        use crate::{rooms::BaseRoomInfo, sync::UnreadNotificationsCount};

        let info = RoomInfo {
            version: 1,
            room_id: room_id!("!gda78o:server.tld").into(),
            room_state: RoomState::Invited,
            prev_room_state: None,
            notification_counts: UnreadNotificationsCount {
                highlight_count: 1,
                notification_count: 2,
            },
            summary: RoomSummary {
                room_heroes: vec![RoomHero {
                    user_id: owned_user_id!("@somebody:example.org"),
                    display_name: None,
                    avatar_url: None,
                }],
                joined_member_count: 5,
                invited_member_count: 0,
            },
            members_synced: true,
            last_prev_batch: Some("pb".to_owned()),
            sync_info: SyncInfo::FullySynced,
            encryption_state_synced: true,
            latest_event: Some(Box::new(LatestEvent::new(SyncTimelineEvent::new(
                Raw::from_json_string(json!({"sender": "@u:i.uk"}).to_string()).unwrap(),
            )))),
            base_info: Box::new(
                assign!(BaseRoomInfo::new(), { pinned_events: Some(RoomPinnedEventsEventContent::new(vec![owned_event_id!("$a")])) }),
            ),
            read_receipts: Default::default(),
            warned_about_unknown_room_version: Arc::new(false.into()),
            cached_display_name: None,
            cached_user_defined_notification_mode: None,
            recency_stamp: Some(42),
        };

        let info_json = json!({
            "version": 1,
            "room_id": "!gda78o:server.tld",
            "room_state": "Invited",
            "prev_room_state": null,
            "notification_counts": {
                "highlight_count": 1,
                "notification_count": 2,
            },
            "summary": {
                "room_heroes": [{
                    "user_id": "@somebody:example.org",
                    "display_name": null,
                    "avatar_url": null
                }],
                "joined_member_count": 5,
                "invited_member_count": 0,
            },
            "members_synced": true,
            "last_prev_batch": "pb",
            "sync_info": "FullySynced",
            "encryption_state_synced": true,
            "latest_event": {
                "event": {
                    "kind": {"PlainText": {"event": {"sender": "@u:i.uk"}}},
                },
            },
            "base_info": {
                "avatar": null,
                "canonical_alias": null,
                "create": null,
                "dm_targets": [],
                "encryption": null,
                "guest_access": null,
                "history_visibility": null,
                "is_marked_unread": false,
                "join_rules": null,
                "max_power_level": 100,
                "name": null,
                "tombstone": null,
                "topic": null,
                "pinned_events": {
                    "pinned": ["$a"]
                },
            },
            "read_receipts": {
                "num_unread": 0,
                "num_mentions": 0,
                "num_notifications": 0,
                "latest_active": null,
                "pending": []
            },
            "recency_stamp": 42,
        });

        assert_eq!(serde_json::to_value(info).unwrap(), info_json);
    }

    // Ensure we can still deserialize RoomInfos before we added things to its
    // schema
    //
    // In an ideal world, we must not change this test. Please see
    // [`test_room_info_serialization`] if you want to test a “recent” `RoomInfo`
    // deserialization.
    #[test]
    fn test_room_info_deserialization_without_optional_items() {
        use ruma::{owned_mxc_uri, owned_user_id};

        // The following JSON should never change if we want to be able to read in old
        // cached state
        let info_json = json!({
            "room_id": "!gda78o:server.tld",
            "room_state": "Invited",
            "prev_room_state": null,
            "notification_counts": {
                "highlight_count": 1,
                "notification_count": 2,
            },
            "summary": {
                "room_heroes": [{
                    "user_id": "@somebody:example.org",
                    "display_name": "Somebody",
                    "avatar_url": "mxc://example.org/abc"
                }],
                "joined_member_count": 5,
                "invited_member_count": 0,
            },
            "members_synced": true,
            "last_prev_batch": "pb",
            "sync_info": "FullySynced",
            "encryption_state_synced": true,
            "base_info": {
                "avatar": null,
                "canonical_alias": null,
                "create": null,
                "dm_targets": [],
                "encryption": null,
                "guest_access": null,
                "history_visibility": null,
                "join_rules": null,
                "max_power_level": 100,
                "name": null,
                "tombstone": null,
                "topic": null,
            },
        });

        let info: RoomInfo = serde_json::from_value(info_json).unwrap();

        assert_eq!(info.room_id, room_id!("!gda78o:server.tld"));
        assert_eq!(info.room_state, RoomState::Invited);
        assert_eq!(info.notification_counts.highlight_count, 1);
        assert_eq!(info.notification_counts.notification_count, 2);
        assert_eq!(
            info.summary.room_heroes,
            vec![RoomHero {
                user_id: owned_user_id!("@somebody:example.org"),
                display_name: Some("Somebody".to_owned()),
                avatar_url: Some(owned_mxc_uri!("mxc://example.org/abc")),
            }]
        );
        assert_eq!(info.summary.joined_member_count, 5);
        assert_eq!(info.summary.invited_member_count, 0);
        assert!(info.members_synced);
        assert_eq!(info.last_prev_batch, Some("pb".to_owned()));
        assert_eq!(info.sync_info, SyncInfo::FullySynced);
        assert!(info.encryption_state_synced);
        assert!(info.base_info.avatar.is_none());
        assert!(info.base_info.canonical_alias.is_none());
        assert!(info.base_info.create.is_none());
        assert_eq!(info.base_info.dm_targets.len(), 0);
        assert!(info.base_info.encryption.is_none());
        assert!(info.base_info.guest_access.is_none());
        assert!(info.base_info.history_visibility.is_none());
        assert!(info.base_info.join_rules.is_none());
        assert_eq!(info.base_info.max_power_level, 100);
        assert!(info.base_info.name.is_none());
        assert!(info.base_info.tombstone.is_none());
        assert!(info.base_info.topic.is_none());
    }

    #[test]
    #[cfg(feature = "experimental-sliding-sync")]
    fn test_room_info_deserialization() {
        use ruma::{owned_mxc_uri, owned_user_id};

        use crate::notification_settings::RoomNotificationMode;

        let info_json = json!({
            "room_id": "!gda78o:server.tld",
            "room_state": "Joined",
            "prev_room_state": "Invited",
            "notification_counts": {
                "highlight_count": 1,
                "notification_count": 2,
            },
            "summary": {
                "room_heroes": [{
                    "user_id": "@somebody:example.org",
                    "display_name": "Somebody",
                    "avatar_url": "mxc://example.org/abc"
                }],
                "joined_member_count": 5,
                "invited_member_count": 0,
            },
            "members_synced": true,
            "last_prev_batch": "pb",
            "sync_info": "FullySynced",
            "encryption_state_synced": true,
            "base_info": {
                "avatar": null,
                "canonical_alias": null,
                "create": null,
                "dm_targets": [],
                "encryption": null,
                "guest_access": null,
                "history_visibility": null,
                "join_rules": null,
                "max_power_level": 100,
                "name": null,
                "tombstone": null,
                "topic": null,
            },
            "cached_display_name": { "Calculated": "lol" },
            "cached_user_defined_notification_mode": "Mute",
            "recency_stamp": 42,
        });

        let info: RoomInfo = serde_json::from_value(info_json).unwrap();

        assert_eq!(info.room_id, room_id!("!gda78o:server.tld"));
        assert_eq!(info.room_state, RoomState::Joined);
        assert_eq!(info.prev_room_state, Some(RoomState::Invited));
        assert_eq!(info.notification_counts.highlight_count, 1);
        assert_eq!(info.notification_counts.notification_count, 2);
        assert_eq!(
            info.summary.room_heroes,
            vec![RoomHero {
                user_id: owned_user_id!("@somebody:example.org"),
                display_name: Some("Somebody".to_owned()),
                avatar_url: Some(owned_mxc_uri!("mxc://example.org/abc")),
            }]
        );
        assert_eq!(info.summary.joined_member_count, 5);
        assert_eq!(info.summary.invited_member_count, 0);
        assert!(info.members_synced);
        assert_eq!(info.last_prev_batch, Some("pb".to_owned()));
        assert_eq!(info.sync_info, SyncInfo::FullySynced);
        assert!(info.encryption_state_synced);
        assert!(info.latest_event.is_none());
        assert!(info.base_info.avatar.is_none());
        assert!(info.base_info.canonical_alias.is_none());
        assert!(info.base_info.create.is_none());
        assert_eq!(info.base_info.dm_targets.len(), 0);
        assert!(info.base_info.encryption.is_none());
        assert!(info.base_info.guest_access.is_none());
        assert!(info.base_info.history_visibility.is_none());
        assert!(info.base_info.join_rules.is_none());
        assert_eq!(info.base_info.max_power_level, 100);
        assert!(info.base_info.name.is_none());
        assert!(info.base_info.tombstone.is_none());
        assert!(info.base_info.topic.is_none());

        assert_eq!(
            info.cached_display_name.as_ref(),
            Some(&RoomDisplayName::Calculated("lol".to_owned())),
        );
        assert_eq!(
            info.cached_user_defined_notification_mode.as_ref(),
            Some(&RoomNotificationMode::Mute)
        );
        assert_eq!(info.recency_stamp.as_ref(), Some(&42));
    }

    #[async_test]
    async fn test_is_favourite() {
        // Given a room,
        let client = BaseClient::with_store_config(StoreConfig::new(
            "cross-process-store-locks-holder-name".to_owned(),
        ));

        client
            .set_session_meta(
                SessionMeta {
                    user_id: user_id!("@alice:example.org").into(),
                    device_id: ruma::device_id!("AYEAYEAYE").into(),
                },
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await
            .unwrap();

        let room_id = room_id!("!test:localhost");
        let room = client.get_or_create_room(room_id, RoomState::Joined);

        // Sanity checks to ensure the room isn't marked as favourite.
        assert!(room.is_favourite().not());

        // Subscribe to the `RoomInfo`.
        let mut room_info_subscriber = room.subscribe_info();

        assert_pending!(room_info_subscriber);

        // Create the tag.
        let tag_raw = Raw::new(&json!({
            "content": {
                "tags": {
                    "m.favourite": {
                        "order": 0.0
                    },
                },
            },
            "type": "m.tag",
        }))
        .unwrap()
        .cast();

        // When the new tag is handled and applied.
        let mut changes = StateChanges::default();
        client
            .handle_room_account_data(room_id, &[tag_raw], &mut changes, &mut Default::default())
            .await;
        client.apply_changes(&changes, Default::default());

        // The `RoomInfo` is getting notified.
        assert_ready!(room_info_subscriber);
        assert_pending!(room_info_subscriber);

        // The room is now marked as favourite.
        assert!(room.is_favourite());

        // Now, let's remove the tag.
        let tag_raw = Raw::new(&json!({
            "content": {
                "tags": {},
            },
            "type": "m.tag"
        }))
        .unwrap()
        .cast();
        client
            .handle_room_account_data(room_id, &[tag_raw], &mut changes, &mut Default::default())
            .await;
        client.apply_changes(&changes, Default::default());

        // The `RoomInfo` is getting notified.
        assert_ready!(room_info_subscriber);
        assert_pending!(room_info_subscriber);

        // The room is now marked as _not_ favourite.
        assert!(room.is_favourite().not());
    }

    #[async_test]
    async fn test_is_low_priority() {
        // Given a room,
        let client = BaseClient::with_store_config(StoreConfig::new(
            "cross-process-store-locks-holder-name".to_owned(),
        ));

        client
            .set_session_meta(
                SessionMeta {
                    user_id: user_id!("@alice:example.org").into(),
                    device_id: ruma::device_id!("AYEAYEAYE").into(),
                },
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await
            .unwrap();

        let room_id = room_id!("!test:localhost");
        let room = client.get_or_create_room(room_id, RoomState::Joined);

        // Sanity checks to ensure the room isn't marked as low priority.
        assert!(!room.is_low_priority());

        // Subscribe to the `RoomInfo`.
        let mut room_info_subscriber = room.subscribe_info();

        assert_pending!(room_info_subscriber);

        // Create the tag.
        let tag_raw = Raw::new(&json!({
            "content": {
                "tags": {
                    "m.lowpriority": {
                        "order": 0.0
                    },
                }
            },
            "type": "m.tag"
        }))
        .unwrap()
        .cast();

        // When the new tag is handled and applied.
        let mut changes = StateChanges::default();
        client
            .handle_room_account_data(room_id, &[tag_raw], &mut changes, &mut Default::default())
            .await;
        client.apply_changes(&changes, Default::default());

        // The `RoomInfo` is getting notified.
        assert_ready!(room_info_subscriber);
        assert_pending!(room_info_subscriber);

        // The room is now marked as low priority.
        assert!(room.is_low_priority());

        // Now, let's remove the tag.
        let tag_raw = Raw::new(&json!({
            "content": {
                "tags": {},
            },
            "type": "m.tag"
        }))
        .unwrap()
        .cast();
        client
            .handle_room_account_data(room_id, &[tag_raw], &mut changes, &mut Default::default())
            .await;
        client.apply_changes(&changes, Default::default());

        // The `RoomInfo` is getting notified.
        assert_ready!(room_info_subscriber);
        assert_pending!(room_info_subscriber);

        // The room is now marked as _not_ low priority.
        assert!(room.is_low_priority().not());
    }

    fn make_room_test_helper(room_type: RoomState) -> (Arc<MemoryStore>, Room) {
        let store = Arc::new(MemoryStore::new());
        let user_id = user_id!("@me:example.org");
        let room_id = room_id!("!test:localhost");
        let (sender, _receiver) = tokio::sync::broadcast::channel(1);

        (store.clone(), Room::new(user_id, store, room_id, room_type, sender))
    }

    fn make_stripped_member_event(user_id: &UserId, name: &str) -> Raw<StrippedRoomMemberEvent> {
        let ev_json = json!({
            "type": "m.room.member",
            "content": assign!(RoomMemberEventContent::new(MembershipState::Join), {
                displayname: Some(name.to_owned())
            }),
            "sender": user_id,
            "state_key": user_id,
        });

        Raw::new(&ev_json).unwrap().cast()
    }

    #[async_test]
    async fn test_display_name_for_joined_room_is_empty_if_no_info() {
        let (_, room) = make_room_test_helper(RoomState::Joined);
        assert_eq!(room.compute_display_name().await.unwrap(), RoomDisplayName::Empty);
    }

    #[async_test]
    async fn test_display_name_for_joined_room_uses_canonical_alias_if_available() {
        let (_, room) = make_room_test_helper(RoomState::Joined);
        room.inner
            .update(|info| info.base_info.canonical_alias = Some(make_canonical_alias_event()));
        assert_eq!(
            room.compute_display_name().await.unwrap(),
            RoomDisplayName::Aliased("test".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_for_joined_room_prefers_name_over_alias() {
        let (_, room) = make_room_test_helper(RoomState::Joined);
        room.inner
            .update(|info| info.base_info.canonical_alias = Some(make_canonical_alias_event()));
        assert_eq!(
            room.compute_display_name().await.unwrap(),
            RoomDisplayName::Aliased("test".to_owned())
        );
        room.inner.update(|info| info.base_info.name = Some(make_name_event()));
        // Display name wasn't cached when we asked for it above, and name overrides
        assert_eq!(
            room.compute_display_name().await.unwrap(),
            RoomDisplayName::Named("Test Room".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_for_invited_room_is_empty_if_no_info() {
        let (_, room) = make_room_test_helper(RoomState::Invited);
        assert_eq!(room.compute_display_name().await.unwrap(), RoomDisplayName::Empty);
    }

    #[async_test]
    async fn test_display_name_for_invited_room_is_empty_if_room_name_empty() {
        let (_, room) = make_room_test_helper(RoomState::Invited);

        let room_name = MinimalStateEvent::Original(OriginalMinimalStateEvent {
            content: RoomNameEventContent::new(String::new()),
            event_id: None,
        });
        room.inner.update(|info| info.base_info.name = Some(room_name));

        assert_eq!(room.compute_display_name().await.unwrap(), RoomDisplayName::Empty);
    }

    #[async_test]
    async fn test_display_name_for_invited_room_uses_canonical_alias_if_available() {
        let (_, room) = make_room_test_helper(RoomState::Invited);
        room.inner
            .update(|info| info.base_info.canonical_alias = Some(make_canonical_alias_event()));
        assert_eq!(
            room.compute_display_name().await.unwrap(),
            RoomDisplayName::Aliased("test".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_for_invited_room_prefers_name_over_alias() {
        let (_, room) = make_room_test_helper(RoomState::Invited);
        room.inner
            .update(|info| info.base_info.canonical_alias = Some(make_canonical_alias_event()));
        assert_eq!(
            room.compute_display_name().await.unwrap(),
            RoomDisplayName::Aliased("test".to_owned())
        );
        room.inner.update(|info| info.base_info.name = Some(make_name_event()));
        // Display name wasn't cached when we asked for it above, and name overrides
        assert_eq!(
            room.compute_display_name().await.unwrap(),
            RoomDisplayName::Named("Test Room".to_owned())
        );
    }

    fn make_canonical_alias_event() -> MinimalStateEvent<RoomCanonicalAliasEventContent> {
        MinimalStateEvent::Original(OriginalMinimalStateEvent {
            content: assign!(RoomCanonicalAliasEventContent::new(), {
                alias: Some(room_alias_id!("#test:example.com").to_owned()),
            }),
            event_id: None,
        })
    }

    fn make_name_event() -> MinimalStateEvent<RoomNameEventContent> {
        MinimalStateEvent::Original(OriginalMinimalStateEvent {
            content: RoomNameEventContent::new("Test Room".to_owned()),
            event_id: None,
        })
    }

    #[async_test]
    async fn test_display_name_dm_invited() {
        let (store, room) = make_room_test_helper(RoomState::Invited);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            heroes: vec![me.to_owned(), matthew.to_owned()],
        });

        changes.add_stripped_member(
            room_id,
            matthew,
            make_stripped_member_event(matthew, "Matthew"),
        );
        changes.add_stripped_member(room_id, me, make_stripped_member_event(me, "Me"));
        store.save_changes(&changes).await.unwrap();

        room.inner.update_if(|info| info.update_from_ruma_summary(&summary));
        assert_eq!(
            room.compute_display_name().await.unwrap(),
            RoomDisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_invited_no_heroes() {
        let (store, room) = make_room_test_helper(RoomState::Invited);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());

        changes.add_stripped_member(
            room_id,
            matthew,
            make_stripped_member_event(matthew, "Matthew"),
        );
        changes.add_stripped_member(room_id, me, make_stripped_member_event(me, "Me"));
        store.save_changes(&changes).await.unwrap();

        assert_eq!(
            room.compute_display_name().await.unwrap(),
            RoomDisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_joined() {
        let (store, room) = make_room_test_helper(RoomState::Joined);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");

        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            joined_member_count: Some(2u32.into()),
            heroes: vec![me.to_owned(), matthew.to_owned()],
        });

        let f = EventFactory::new().room(room_id!("!test:localhost"));

        let members = changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        members.insert(matthew.into(), f.member(matthew).display_name("Matthew").into_raw());
        members.insert(me.into(), f.member(me).display_name("Me").into_raw());

        store.save_changes(&changes).await.unwrap();

        room.inner.update_if(|info| info.update_from_ruma_summary(&summary));
        assert_eq!(
            room.compute_display_name().await.unwrap(),
            RoomDisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_joined_no_heroes() {
        let (store, room) = make_room_test_helper(RoomState::Joined);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());

        let f = EventFactory::new().room(room_id!("!test:localhost"));

        let members = changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        members.insert(matthew.into(), f.member(matthew).display_name("Matthew").into_raw());
        members.insert(me.into(), f.member(me).display_name("Me").into_raw());

        store.save_changes(&changes).await.unwrap();

        assert_eq!(
            room.compute_display_name().await.unwrap(),
            RoomDisplayName::Calculated("Matthew".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_deterministic() {
        let (store, room) = make_room_test_helper(RoomState::Joined);

        let alice = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");
        let carol = user_id!("@carol:example.org");
        let denis = user_id!("@denis:example.org");
        let erica = user_id!("@erica:example.org");
        let fred = user_id!("@fred:example.org");
        let me = user_id!("@me:example.org");

        let mut changes = StateChanges::new("".to_owned());

        let f = EventFactory::new().room(room_id!("!test:localhost"));

        // Save members in two batches, so that there's no implied ordering in the
        // store.
        {
            let members = changes
                .state
                .entry(room.room_id().to_owned())
                .or_default()
                .entry(StateEventType::RoomMember)
                .or_default();
            members.insert(carol.into(), f.member(carol).display_name("Carol").into_raw());
            members.insert(bob.into(), f.member(bob).display_name("Bob").into_raw());
            members.insert(fred.into(), f.member(fred).display_name("Fred").into_raw());
            members.insert(me.into(), f.member(me).display_name("Me").into_raw());
            store.save_changes(&changes).await.unwrap();
        }

        {
            let members = changes
                .state
                .entry(room.room_id().to_owned())
                .or_default()
                .entry(StateEventType::RoomMember)
                .or_default();
            members.insert(alice.into(), f.member(alice).display_name("Alice").into_raw());
            members.insert(erica.into(), f.member(erica).display_name("Erica").into_raw());
            members.insert(denis.into(), f.member(denis).display_name("Denis").into_raw());
            store.save_changes(&changes).await.unwrap();
        }

        let summary = assign!(RumaSummary::new(), {
            joined_member_count: Some(7u32.into()),
            heroes: vec![denis.to_owned(), carol.to_owned(), bob.to_owned(), erica.to_owned()],
        });
        room.inner.update_if(|info| info.update_from_ruma_summary(&summary));

        assert_eq!(
            room.compute_display_name().await.unwrap(),
            RoomDisplayName::Calculated("Bob, Carol, Denis, Erica, and 3 others".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_deterministic_no_heroes() {
        let (store, room) = make_room_test_helper(RoomState::Joined);

        let alice = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");
        let carol = user_id!("@carol:example.org");
        let denis = user_id!("@denis:example.org");
        let erica = user_id!("@erica:example.org");
        let fred = user_id!("@fred:example.org");
        let me = user_id!("@me:example.org");

        let f = EventFactory::new().room(room_id!("!test:localhost"));

        let mut changes = StateChanges::new("".to_owned());

        // Save members in two batches, so that there's no implied ordering in the
        // store.
        {
            let members = changes
                .state
                .entry(room.room_id().to_owned())
                .or_default()
                .entry(StateEventType::RoomMember)
                .or_default();
            members.insert(carol.into(), f.member(carol).display_name("Carol").into_raw());
            members.insert(bob.into(), f.member(bob).display_name("Bob").into_raw());
            members.insert(fred.into(), f.member(fred).display_name("Fred").into_raw());
            members.insert(me.into(), f.member(me).display_name("Me").into_raw());

            store.save_changes(&changes).await.unwrap();
        }

        {
            let members = changes
                .state
                .entry(room.room_id().to_owned())
                .or_default()
                .entry(StateEventType::RoomMember)
                .or_default();
            members.insert(alice.into(), f.member(alice).display_name("Alice").into_raw());
            members.insert(erica.into(), f.member(erica).display_name("Erica").into_raw());
            members.insert(denis.into(), f.member(denis).display_name("Denis").into_raw());
            store.save_changes(&changes).await.unwrap();
        }

        assert_eq!(
            room.compute_display_name().await.unwrap(),
            RoomDisplayName::Calculated("Alice, Bob, Carol, Denis, Erica, and 2 others".to_owned())
        );
    }

    #[async_test]
    async fn test_display_name_dm_alone() {
        let (store, room) = make_room_test_helper(RoomState::Joined);
        let room_id = room_id!("!test:localhost");
        let matthew = user_id!("@matthew:example.org");
        let me = user_id!("@me:example.org");
        let mut changes = StateChanges::new("".to_owned());
        let summary = assign!(RumaSummary::new(), {
            joined_member_count: Some(1u32.into()),
            heroes: vec![me.to_owned(), matthew.to_owned()],
        });

        let f = EventFactory::new().room(room_id!("!test:localhost"));

        let members = changes
            .state
            .entry(room_id.to_owned())
            .or_default()
            .entry(StateEventType::RoomMember)
            .or_default();
        members.insert(matthew.into(), f.member(matthew).display_name("Matthew").into_raw());
        members.insert(me.into(), f.member(me).display_name("Me").into_raw());

        store.save_changes(&changes).await.unwrap();

        room.inner.update_if(|info| info.update_from_ruma_summary(&summary));
        assert_eq!(
            room.compute_display_name().await.unwrap(),
            RoomDisplayName::EmptyWas("Matthew".to_owned())
        );
    }

    #[async_test]
    #[cfg(feature = "experimental-sliding-sync")]
    async fn test_setting_the_latest_event_doesnt_cause_a_room_info_notable_update() {
        use std::collections::BTreeMap;

        use assert_matches::assert_matches;

        use crate::{RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons};

        // Given a room,
        let client = BaseClient::with_store_config(StoreConfig::new(
            "cross-process-store-locks-holder-name".to_owned(),
        ));

        client
            .set_session_meta(
                SessionMeta {
                    user_id: user_id!("@alice:example.org").into(),
                    device_id: ruma::device_id!("AYEAYEAYE").into(),
                },
                #[cfg(feature = "e2e-encryption")]
                None,
            )
            .await
            .unwrap();

        let room_id = room_id!("!test:localhost");
        let room = client.get_or_create_room(room_id, RoomState::Joined);

        // That has an encrypted event,
        add_encrypted_event(&room, "$A");
        // Sanity: it has no latest_event
        assert!(room.latest_event().is_none());

        // When I set up an observer on the latest_event,
        let mut room_info_notable_update = client.room_info_notable_update_receiver();

        // And I provide a decrypted event to replace the encrypted one,
        let event = make_latest_event("$A");

        let mut changes = StateChanges::default();
        let mut room_info_notable_updates = BTreeMap::new();
        room.on_latest_event_decrypted(
            event.clone(),
            0,
            &mut changes,
            &mut room_info_notable_updates,
        );

        assert!(room_info_notable_updates.contains_key(room_id));

        // The subscriber isn't notified at this point.
        assert!(room_info_notable_update.try_recv().is_err());

        // Then updating the room info will store the event,
        client.apply_changes(&changes, room_info_notable_updates);
        assert_eq!(room.latest_event().unwrap().event_id(), event.event_id());

        // And wake up the subscriber.
        assert_matches!(
            room_info_notable_update.recv().await,
            Ok(RoomInfoNotableUpdate { room_id: received_room_id, reasons }) => {
                assert_eq!(received_room_id, room_id);
                assert!(reasons.contains(RoomInfoNotableUpdateReasons::LATEST_EVENT));
            }
        );
    }

    #[async_test]
    #[cfg(feature = "experimental-sliding-sync")]
    async fn test_when_we_provide_a_newly_decrypted_event_it_replaces_latest_event() {
        use std::collections::BTreeMap;

        // Given a room with an encrypted event
        let (_store, room) = make_room_test_helper(RoomState::Joined);
        add_encrypted_event(&room, "$A");
        // Sanity: it has no latest_event
        assert!(room.latest_event().is_none());

        // When I provide a decrypted event to replace the encrypted one
        let event = make_latest_event("$A");
        let mut changes = StateChanges::default();
        let mut room_info_notable_updates = BTreeMap::new();
        room.on_latest_event_decrypted(
            event.clone(),
            0,
            &mut changes,
            &mut room_info_notable_updates,
        );
        room.set_room_info(
            changes.room_infos.get(room.room_id()).cloned().unwrap(),
            room_info_notable_updates.get(room.room_id()).copied().unwrap(),
        );

        // Then is it stored
        assert_eq!(room.latest_event().unwrap().event_id(), event.event_id());
    }

    #[async_test]
    #[cfg(feature = "experimental-sliding-sync")]
    async fn test_when_a_newly_decrypted_event_appears_we_delete_all_older_encrypted_events() {
        use std::collections::BTreeMap;

        // Given a room with some encrypted events and a latest event
        let (_store, room) = make_room_test_helper(RoomState::Joined);
        room.inner.update(|info| info.latest_event = Some(make_latest_event("$A")));
        add_encrypted_event(&room, "$0");
        add_encrypted_event(&room, "$1");
        add_encrypted_event(&room, "$2");
        add_encrypted_event(&room, "$3");

        // When I provide a latest event
        let new_event = make_latest_event("$1");
        let new_event_index = 1;
        let mut changes = StateChanges::default();
        let mut room_info_notable_updates = BTreeMap::new();
        room.on_latest_event_decrypted(
            new_event.clone(),
            new_event_index,
            &mut changes,
            &mut room_info_notable_updates,
        );
        room.set_room_info(
            changes.room_infos.get(room.room_id()).cloned().unwrap(),
            room_info_notable_updates.get(room.room_id()).copied().unwrap(),
        );

        // Then the encrypted events list is shortened to only newer events
        let enc_evs = room.latest_encrypted_events();
        assert_eq!(enc_evs.len(), 2);
        assert_eq!(enc_evs[0].get_field::<&str>("event_id").unwrap().unwrap(), "$2");
        assert_eq!(enc_evs[1].get_field::<&str>("event_id").unwrap().unwrap(), "$3");

        // And the event is stored
        assert_eq!(room.latest_event().unwrap().event_id(), new_event.event_id());
    }

    #[async_test]
    #[cfg(feature = "experimental-sliding-sync")]
    async fn test_replacing_the_newest_event_leaves_none_left() {
        use std::collections::BTreeMap;

        // Given a room with some encrypted events
        let (_store, room) = make_room_test_helper(RoomState::Joined);
        add_encrypted_event(&room, "$0");
        add_encrypted_event(&room, "$1");
        add_encrypted_event(&room, "$2");
        add_encrypted_event(&room, "$3");

        // When I provide a latest event and say it was the very latest
        let new_event = make_latest_event("$3");
        let new_event_index = 3;
        let mut changes = StateChanges::default();
        let mut room_info_notable_updates = BTreeMap::new();
        room.on_latest_event_decrypted(
            new_event,
            new_event_index,
            &mut changes,
            &mut room_info_notable_updates,
        );
        room.set_room_info(
            changes.room_infos.get(room.room_id()).cloned().unwrap(),
            room_info_notable_updates.get(room.room_id()).copied().unwrap(),
        );

        // Then the encrypted events list ie empty
        let enc_evs = room.latest_encrypted_events();
        assert_eq!(enc_evs.len(), 0);
    }

    #[cfg(feature = "experimental-sliding-sync")]
    fn add_encrypted_event(room: &Room, event_id: &str) {
        room.latest_encrypted_events
            .write()
            .unwrap()
            .push(Raw::from_json_string(json!({ "event_id": event_id }).to_string()).unwrap());
    }

    #[cfg(feature = "experimental-sliding-sync")]
    fn make_latest_event(event_id: &str) -> Box<LatestEvent> {
        Box::new(LatestEvent::new(SyncTimelineEvent::new(
            Raw::from_json_string(json!({ "event_id": event_id }).to_string()).unwrap(),
        )))
    }

    fn timestamp(minutes_ago: u32) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch::from_system_time(
            SystemTime::now().sub(Duration::from_secs((60 * minutes_ago).into())),
        )
        .expect("date out of range")
    }

    fn legacy_membership_for_my_call(
        device_id: &DeviceId,
        membership_id: &str,
        minutes_ago: u32,
    ) -> LegacyMembershipData {
        let (application, foci) = foci_and_application();
        assign!(
            LegacyMembershipData::from(LegacyMembershipDataInit {
                application,
                device_id: device_id.to_owned(),
                expires: Duration::from_millis(3_600_000),
                foci_active: foci,
                membership_id: membership_id.to_owned(),
            }),
            { created_ts: Some(timestamp(minutes_ago)) }
        )
    }

    fn legacy_member_state_event(
        memberships: Vec<LegacyMembershipData>,
        ev_id: &EventId,
        user_id: &UserId,
    ) -> AnySyncStateEvent {
        let content = CallMemberEventContent::new_legacy(memberships);

        AnySyncStateEvent::CallMember(SyncStateEvent::Original(OriginalSyncCallMemberEvent {
            content,
            event_id: ev_id.to_owned(),
            sender: user_id.to_owned(),
            // we can simply use now here since this will be dropped when using a MinimalStateEvent
            // in the roomInfo
            origin_server_ts: timestamp(0),
            state_key: CallMemberStateKey::new(user_id.to_owned(), None, false),
            unsigned: StateUnsigned::new(),
        }))
    }

    struct InitData<'a> {
        device_id: &'a DeviceId,
        minutes_ago: u32,
    }

    fn session_member_state_event(
        ev_id: &EventId,
        user_id: &UserId,
        init_data: Option<InitData<'_>>,
    ) -> AnySyncStateEvent {
        let application = Application::Call(CallApplicationContent::new(
            "my_call_id_1".to_owned(),
            ruma::events::call::member::CallScope::Room,
        ));
        let foci_preferred = vec![Focus::Livekit(LivekitFocus::new(
            "my_call_foci_alias".to_owned(),
            "https://lk.org".to_owned(),
        ))];
        let focus_active = ActiveFocus::Livekit(ActiveLivekitFocus::new());
        let (content, state_key) = match init_data {
            Some(InitData { device_id, minutes_ago }) => (
                CallMemberEventContent::new(
                    application,
                    device_id.to_owned(),
                    focus_active,
                    foci_preferred,
                    Some(timestamp(minutes_ago)),
                ),
                CallMemberStateKey::new(user_id.to_owned(), Some(device_id.to_owned()), false),
            ),
            None => (
                CallMemberEventContent::new_empty(None),
                CallMemberStateKey::new(user_id.to_owned(), None, false),
            ),
        };

        AnySyncStateEvent::CallMember(SyncStateEvent::Original(OriginalSyncCallMemberEvent {
            content,
            event_id: ev_id.to_owned(),
            sender: user_id.to_owned(),
            // we can simply use now here since this will be dropped when using a MinimalStateEvent
            // in the roomInfo
            origin_server_ts: timestamp(0),
            state_key,
            unsigned: StateUnsigned::new(),
        }))
    }

    fn foci_and_application() -> (Application, Vec<Focus>) {
        (
            Application::Call(CallApplicationContent::new(
                "my_call_id_1".to_owned(),
                ruma::events::call::member::CallScope::Room,
            )),
            vec![Focus::Livekit(LivekitFocus::new(
                "my_call_foci_alias".to_owned(),
                "https://lk.org".to_owned(),
            ))],
        )
    }

    fn receive_state_events(room: &Room, events: Vec<&AnySyncStateEvent>) {
        room.inner.update_if(|info| {
            let mut res = false;
            for ev in events {
                res |= info.handle_state_event(ev);
            }
            res
        });
    }

    /// `user_a`: empty memberships
    /// `user_b`: one membership
    /// `user_c`: two memberships (two devices)
    fn legacy_create_call_with_member_events_for_user(a: &UserId, b: &UserId, c: &UserId) -> Room {
        let (_, room) = make_room_test_helper(RoomState::Joined);

        let a_empty = legacy_member_state_event(Vec::new(), event_id!("$1234"), a);

        // make b 10min old
        let m_init_b = legacy_membership_for_my_call(device_id!("DEVICE_0"), "0", 1);
        let b_one = legacy_member_state_event(vec![m_init_b], event_id!("$12345"), b);

        // c1 1min old
        let m_init_c1 = legacy_membership_for_my_call(device_id!("DEVICE_0"), "0", 10);
        // c2 20min old
        let m_init_c2 = legacy_membership_for_my_call(device_id!("DEVICE_1"), "0", 20);
        let c_two = legacy_member_state_event(vec![m_init_c1, m_init_c2], event_id!("$123456"), c);

        // Intentionally use a non time sorted receive order.
        receive_state_events(&room, vec![&c_two, &a_empty, &b_one]);

        room
    }

    /// `user_a`: empty memberships
    /// `user_b`: one membership
    /// `user_c`: two memberships (two devices)
    fn session_create_call_with_member_events_for_user(a: &UserId, b: &UserId, c: &UserId) -> Room {
        let (_, room) = make_room_test_helper(RoomState::Joined);

        let a_empty = session_member_state_event(event_id!("$1234"), a, None);

        // make b 10min old
        let b_one = session_member_state_event(
            event_id!("$12345"),
            b,
            Some(InitData { device_id: "DEVICE_0".into(), minutes_ago: 1 }),
        );

        let m_c1 = session_member_state_event(
            event_id!("$123456_0"),
            c,
            Some(InitData { device_id: "DEVICE_0".into(), minutes_ago: 10 }),
        );
        let m_c2 = session_member_state_event(
            event_id!("$123456_1"),
            c,
            Some(InitData { device_id: "DEVICE_1".into(), minutes_ago: 20 }),
        );
        // Intentionally use a non time sorted receive order1
        receive_state_events(&room, vec![&m_c1, &m_c2, &a_empty, &b_one]);

        room
    }

    #[test]
    fn test_show_correct_active_call_state() {
        let room_legacy = legacy_create_call_with_member_events_for_user(&ALICE, &BOB, &CAROL);

        // This check also tests the ordering.
        // We want older events to be in the front.
        // user_b (Bob) is 1min old, c1 (CAROL) 10min old, c2 (CAROL) 20min old
        assert_eq!(
            vec![CAROL.to_owned(), CAROL.to_owned(), BOB.to_owned()],
            room_legacy.active_room_call_participants()
        );
        assert!(room_legacy.has_active_room_call());

        let room_session = session_create_call_with_member_events_for_user(&ALICE, &BOB, &CAROL);
        assert_eq!(
            vec![CAROL.to_owned(), CAROL.to_owned(), BOB.to_owned()],
            room_session.active_room_call_participants()
        );
        assert!(room_session.has_active_room_call());
    }

    #[test]
    fn test_active_call_is_false_when_everyone_left() {
        let room = legacy_create_call_with_member_events_for_user(&ALICE, &BOB, &CAROL);

        let b_empty_membership = legacy_member_state_event(Vec::new(), event_id!("$1234_1"), &BOB);
        let c_empty_membership =
            legacy_member_state_event(Vec::new(), event_id!("$12345_1"), &CAROL);

        receive_state_events(&room, vec![&b_empty_membership, &c_empty_membership]);

        // We have no active call anymore after emptying the memberships
        assert_eq!(Vec::<OwnedUserId>::new(), room.active_room_call_participants());
        assert!(!room.has_active_room_call());
    }

    #[test]
    fn test_calculate_room_name() {
        let mut actual = compute_display_name_from_heroes(2, vec!["a"]);
        assert_eq!(RoomDisplayName::Calculated("a".to_owned()), actual);

        actual = compute_display_name_from_heroes(3, vec!["a", "b"]);
        assert_eq!(RoomDisplayName::Calculated("a, b".to_owned()), actual);

        actual = compute_display_name_from_heroes(4, vec!["a", "b", "c"]);
        assert_eq!(RoomDisplayName::Calculated("a, b, c".to_owned()), actual);

        actual = compute_display_name_from_heroes(5, vec!["a", "b", "c"]);
        assert_eq!(RoomDisplayName::Calculated("a, b, c, and 2 others".to_owned()), actual);

        actual = compute_display_name_from_heroes(5, vec![]);
        assert_eq!(RoomDisplayName::Calculated("5 people".to_owned()), actual);

        actual = compute_display_name_from_heroes(0, vec![]);
        assert_eq!(RoomDisplayName::Empty, actual);

        actual = compute_display_name_from_heroes(1, vec![]);
        assert_eq!(RoomDisplayName::Empty, actual);

        actual = compute_display_name_from_heroes(1, vec!["a"]);
        assert_eq!(RoomDisplayName::EmptyWas("a".to_owned()), actual);

        actual = compute_display_name_from_heroes(1, vec!["a", "b"]);
        assert_eq!(RoomDisplayName::EmptyWas("a, b".to_owned()), actual);

        actual = compute_display_name_from_heroes(1, vec!["a", "b", "c"]);
        assert_eq!(RoomDisplayName::EmptyWas("a, b, c".to_owned()), actual);
    }

    #[test]
    fn test_encryption_is_set_when_encryption_event_is_received() {
        let (_store, room) = make_room_test_helper(RoomState::Joined);

        assert!(room.is_encryption_state_synced().not());
        assert!(room.is_encrypted().not());

        let encryption_content =
            RoomEncryptionEventContent::new(EventEncryptionAlgorithm::MegolmV1AesSha2);
        let encryption_event = AnySyncStateEvent::RoomEncryption(SyncStateEvent::Original(
            OriginalSyncRoomEncryptionEvent {
                content: encryption_content,
                event_id: OwnedEventId::from_str("$1234_1").unwrap(),
                sender: ALICE.to_owned(),
                // we can simply use now here since this will be dropped when using a
                // MinimalStateEvent in the roomInfo
                origin_server_ts: timestamp(0),
                state_key: EmptyStateKey,
                unsigned: StateUnsigned::new(),
            },
        ));
        receive_state_events(&room, vec![&encryption_event]);

        assert!(room.is_encryption_state_synced());
        assert!(room.is_encrypted());
    }

    #[async_test]
    async fn test_room_info_migration_v1() {
        let store = MemoryStore::new().into_state_store();

        let room_info_json = json!({
            "room_id": "!gda78o:server.tld",
            "room_state": "Joined",
            "notification_counts": {
                "highlight_count": 1,
                "notification_count": 2,
            },
            "summary": {
                "room_heroes": [{
                    "user_id": "@somebody:example.org",
                    "display_name": null,
                    "avatar_url": null
                }],
                "joined_member_count": 5,
                "invited_member_count": 0,
            },
            "members_synced": true,
            "last_prev_batch": "pb",
            "sync_info": "FullySynced",
            "encryption_state_synced": true,
            "latest_event": {
                "event": {
                    "encryption_info": null,
                    "event": {
                        "sender": "@u:i.uk",
                    },
                },
            },
            "base_info": {
                "avatar": null,
                "canonical_alias": null,
                "create": null,
                "dm_targets": [],
                "encryption": null,
                "guest_access": null,
                "history_visibility": null,
                "join_rules": null,
                "max_power_level": 100,
                "name": null,
                "tombstone": null,
                "topic": null,
            },
            "read_receipts": {
                "num_unread": 0,
                "num_mentions": 0,
                "num_notifications": 0,
                "latest_active": null,
                "pending": []
            },
            "recency_stamp": 42,
        });
        let mut room_info: RoomInfo = serde_json::from_value(room_info_json).unwrap();

        assert_eq!(room_info.version, 0);
        assert!(room_info.base_info.notable_tags.is_empty());
        assert!(room_info.base_info.pinned_events.is_none());

        // Apply migrations with an empty store.
        assert!(room_info.apply_migrations(store.clone()).await);

        assert_eq!(room_info.version, 1);
        assert!(room_info.base_info.notable_tags.is_empty());
        assert!(room_info.base_info.pinned_events.is_none());

        // Applying migrations again has no effect.
        assert!(!room_info.apply_migrations(store.clone()).await);

        assert_eq!(room_info.version, 1);
        assert!(room_info.base_info.notable_tags.is_empty());
        assert!(room_info.base_info.pinned_events.is_none());

        // Add events to the store.
        let mut changes = StateChanges::default();

        let raw_tag_event = Raw::new(&*TAG).unwrap().cast();
        let tag_event = raw_tag_event.deserialize().unwrap();
        changes.add_room_account_data(&room_info.room_id, tag_event, raw_tag_event);

        let raw_pinned_events_event = Raw::new(&*PINNED_EVENTS).unwrap().cast();
        let pinned_events_event = raw_pinned_events_event.deserialize().unwrap();
        changes.add_state_event(&room_info.room_id, pinned_events_event, raw_pinned_events_event);

        store.save_changes(&changes).await.unwrap();

        // Reset to version 0 and reapply migrations.
        room_info.version = 0;
        assert!(room_info.apply_migrations(store.clone()).await);

        assert_eq!(room_info.version, 1);
        assert!(room_info.base_info.notable_tags.contains(RoomNotableTags::FAVOURITE));
        assert!(room_info.base_info.pinned_events.is_some());

        // Creating a new room info initializes it to version 1.
        let new_room_info = RoomInfo::new(room_id!("!new_room:localhost"), RoomState::Joined);
        assert_eq!(new_room_info.version, 1);
    }

    #[async_test]
    async fn test_prev_room_state_is_updated() {
        let (_store, room) = make_room_test_helper(RoomState::Invited);
        assert_eq!(room.prev_state(), None);
        assert_eq!(room.state(), RoomState::Invited);

        // Invited -> Joined
        let mut room_info = room.clone_info();
        room_info.mark_as_joined();
        room.set_room_info(room_info, RoomInfoNotableUpdateReasons::MEMBERSHIP);
        assert_eq!(room.prev_state(), Some(RoomState::Invited));
        assert_eq!(room.state(), RoomState::Joined);

        // No change when the same state is used
        let mut room_info = room.clone_info();
        room_info.mark_as_joined();
        room.set_room_info(room_info, RoomInfoNotableUpdateReasons::MEMBERSHIP);
        assert_eq!(room.prev_state(), Some(RoomState::Invited));
        assert_eq!(room.state(), RoomState::Joined);

        // Joined -> Left
        let mut room_info = room.clone_info();
        room_info.mark_as_left();
        room.set_room_info(room_info, RoomInfoNotableUpdateReasons::MEMBERSHIP);
        assert_eq!(room.prev_state(), Some(RoomState::Joined));
        assert_eq!(room.state(), RoomState::Left);
    }
}
