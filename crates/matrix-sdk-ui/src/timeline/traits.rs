// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use async_trait::async_trait;
use indexmap::IndexMap;
use matrix_sdk::Room;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk::{deserialized_responses::TimelineEvent, Result};
use matrix_sdk_base::latest_event::LatestEvent;
use ruma::{
    events::receipt::{Receipt, ReceiptThread, ReceiptType},
    push::{PushConditionRoomCtx, Ruleset},
    EventId, OwnedEventId, OwnedUserId, RoomVersionId, UserId,
};
#[cfg(feature = "e2e-encryption")]
use ruma::{events::AnySyncTimelineEvent, serde::Raw};
use tracing::{debug, error, warn};

use super::{Profile, TimelineBuilder};
use crate::timeline::Timeline;

#[async_trait]
pub trait RoomExt {
    /// Get a [`Timeline`] for this room.
    ///
    /// This offers a higher-level API than event handlers, in treating things
    /// like edits and reactions as updates of existing items rather than new
    /// independent events.
    ///
    /// This is the same as using `room.timeline_builder().build()`.
    async fn timeline(&self) -> Timeline;

    /// Get a [`TimelineBuilder`] for this room.
    ///
    /// [`Timeline`] offers a higher-level API than event handlers, in treating
    /// things like edits and reactions as updates of existing items rather
    /// than new independent events.
    ///
    /// This allows to customize settings of the [`Timeline`] before
    /// constructing it.
    fn timeline_builder(&self) -> TimelineBuilder;
}

#[async_trait]
impl RoomExt for Room {
    async fn timeline(&self) -> Timeline {
        self.timeline_builder().build().await
    }

    fn timeline_builder(&self) -> TimelineBuilder {
        Timeline::builder(self).track_read_marker_and_receipts()
    }
}

#[async_trait]
pub(super) trait RoomDataProvider: Clone + Send + Sync + 'static {
    fn own_user_id(&self) -> &UserId;
    fn room_version(&self) -> RoomVersionId;
    async fn profile_from_user_id(&self, user_id: &UserId) -> Option<Profile>;
    async fn profile_from_latest_event(&self, latest_event: &LatestEvent) -> Option<Profile>;
    async fn user_receipt(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Option<(OwnedEventId, Receipt)>;
    async fn read_receipts_for_event(&self, event_id: &EventId) -> IndexMap<OwnedUserId, Receipt>;
    async fn push_rules_and_context(&self) -> Option<(Ruleset, PushConditionRoomCtx)>;
}

#[async_trait]
impl RoomDataProvider for Room {
    fn own_user_id(&self) -> &UserId {
        (**self).own_user_id()
    }

    fn room_version(&self) -> RoomVersionId {
        (**self).clone_info().room_version().cloned().unwrap_or_else(|| {
            warn!("Unknown room version, falling back to v10");
            RoomVersionId::V10
        })
    }

    async fn profile_from_user_id(&self, user_id: &UserId) -> Option<Profile> {
        match self.get_member_no_sync(user_id).await {
            Ok(Some(member)) => Some(Profile {
                display_name: member.display_name().map(ToOwned::to_owned),
                display_name_ambiguous: member.name_ambiguous(),
                avatar_url: member.avatar_url().map(ToOwned::to_owned),
            }),
            Ok(None) if self.are_members_synced() => Some(Profile::default()),
            Ok(None) => None,
            Err(e) => {
                error!(%user_id, "Failed to fetch room member information: {e}");
                None
            }
        }
    }

    async fn profile_from_latest_event(&self, latest_event: &LatestEvent) -> Option<Profile> {
        if !latest_event.has_sender_profile() {
            return None;
        }

        Some(Profile {
            display_name: latest_event.sender_display_name().map(ToOwned::to_owned),
            display_name_ambiguous: latest_event.sender_name_ambiguous().unwrap_or(false),
            avatar_url: latest_event.sender_avatar_url().map(ToOwned::to_owned),
        })
    }

    async fn user_receipt(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Option<(OwnedEventId, Receipt)> {
        match self.user_receipt(receipt_type.clone(), thread.clone(), user_id).await {
            Ok(receipt) => receipt,
            Err(e) => {
                error!(
                    ?receipt_type,
                    ?thread,
                    ?user_id,
                    "Failed to get read receipt for user: {e}"
                );
                None
            }
        }
    }

    async fn read_receipts_for_event(&self, event_id: &EventId) -> IndexMap<OwnedUserId, Receipt> {
        match self.event_receipts(ReceiptType::Read, ReceiptThread::Unthreaded, event_id).await {
            Ok(receipts) => receipts.into_iter().collect(),
            Err(e) => {
                error!(?event_id, "Failed to get read receipts for event: {e}");
                IndexMap::new()
            }
        }
    }

    async fn push_rules_and_context(&self) -> Option<(Ruleset, PushConditionRoomCtx)> {
        match self.push_context().await {
            Ok(Some(push_context)) => match self.client().account().push_rules().await {
                Ok(push_rules) => Some((push_rules, push_context)),
                Err(e) => {
                    error!("Could not get push rules: {e}");
                    None
                }
            },
            Ok(None) => {
                debug!("Could not aggregate push context");
                None
            }
            Err(e) => {
                error!("Could not get push context: {e}");
                None
            }
        }
    }
}

// Internal helper to make most of retry_event_decryption independent of a room
// object, which is annoying to create for testing and not really needed
#[cfg(feature = "e2e-encryption")]
#[async_trait]
pub(super) trait Decryptor: Clone + Send + Sync + 'static {
    async fn decrypt_event_impl(&self, raw: &Raw<AnySyncTimelineEvent>) -> Result<TimelineEvent>;
}

#[cfg(feature = "e2e-encryption")]
#[async_trait]
impl Decryptor for Room {
    async fn decrypt_event_impl(&self, raw: &Raw<AnySyncTimelineEvent>) -> Result<TimelineEvent> {
        self.decrypt_event(raw.cast_ref()).await
    }
}

#[cfg(all(test, feature = "e2e-encryption"))]
#[async_trait]
impl Decryptor for (matrix_sdk_base::crypto::OlmMachine, ruma::OwnedRoomId) {
    async fn decrypt_event_impl(&self, raw: &Raw<AnySyncTimelineEvent>) -> Result<TimelineEvent> {
        let (olm_machine, room_id) = self;
        let event = olm_machine.decrypt_room_event(raw.cast_ref(), room_id).await?;
        Ok(event)
    }
}
