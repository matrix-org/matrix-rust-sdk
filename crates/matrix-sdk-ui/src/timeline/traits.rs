// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::{future::Future, sync::Arc};

use eyeball::Subscriber;
use indexmap::IndexMap;
#[cfg(test)]
use matrix_sdk::crypto::{DecryptionSettings, RoomEventDecryptionResult, TrustRequirement};
use matrix_sdk::{
    crypto::types::events::CryptoContextInfo,
    deserialized_responses::{EncryptionInfo, TimelineEvent},
    event_cache::paginator::PaginableRoom,
    room::{PushContext, Relations, RelationsOptions},
    AsyncTraitDeps, Result, Room, SendOutsideWasm,
};
use matrix_sdk_base::{latest_event::LatestEvent, RoomInfo};
use ruma::{
    events::{
        fully_read::FullyReadEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        AnyMessageLikeEventContent, AnySyncTimelineEvent,
    },
    serde::Raw,
    EventId, OwnedEventId, OwnedTransactionId, OwnedUserId, RoomVersionId, UserId,
};
use tracing::error;

use super::{EventTimelineItem, Profile, RedactError, TimelineBuilder};
use crate::timeline::{self, pinned_events_loader::PinnedEventsRoom, Timeline};

pub trait RoomExt {
    /// Get a [`Timeline`] for this room.
    ///
    /// This offers a higher-level API than event handlers, in treating things
    /// like edits and reactions as updates of existing items rather than new
    /// independent events.
    ///
    /// This is the same as using `room.timeline_builder().build()`.
    fn timeline(&self)
        -> impl Future<Output = Result<Timeline, timeline::Error>> + SendOutsideWasm;

    /// Get a [`TimelineBuilder`] for this room.
    ///
    /// [`Timeline`] offers a higher-level API than event handlers, in treating
    /// things like edits and reactions as updates of existing items rather
    /// than new independent events.
    ///
    /// This allows to customize settings of the [`Timeline`] before
    /// constructing it.
    fn timeline_builder(&self) -> TimelineBuilder;

    /// Return an optional [`EventTimelineItem`] corresponding to this room's
    /// latest event.
    fn latest_event_item(
        &self,
    ) -> impl Future<Output = Option<EventTimelineItem>> + SendOutsideWasm;
}

impl RoomExt for Room {
    async fn timeline(&self) -> Result<Timeline, timeline::Error> {
        self.timeline_builder().build().await
    }

    fn timeline_builder(&self) -> TimelineBuilder {
        TimelineBuilder::new(self).track_read_marker_and_receipts()
    }

    async fn latest_event_item(&self) -> Option<EventTimelineItem> {
        if let Some(latest_event) = (**self).latest_event() {
            EventTimelineItem::from_latest_event(self.client(), self.room_id(), latest_event).await
        } else {
            None
        }
    }
}

pub(super) trait RoomDataProvider:
    Clone + PaginableRoom + PinnedEventsRoom + 'static
{
    fn own_user_id(&self) -> &UserId;
    fn room_version(&self) -> RoomVersionId;

    fn crypto_context_info(&self)
        -> impl Future<Output = CryptoContextInfo> + SendOutsideWasm + '_;

    fn profile_from_user_id<'a>(
        &'a self,
        user_id: &'a UserId,
    ) -> impl Future<Output = Option<Profile>> + SendOutsideWasm + 'a;
    fn profile_from_latest_event(&self, latest_event: &LatestEvent) -> Option<Profile>;

    /// Loads a user receipt from the storage backend.
    fn load_user_receipt<'a>(
        &'a self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &'a UserId,
    ) -> impl Future<Output = Option<(OwnedEventId, Receipt)>> + SendOutsideWasm + 'a;

    /// Loads read receipts for an event from the storage backend.
    fn load_event_receipts<'a>(
        &'a self,
        event_id: &'a EventId,
    ) -> impl Future<Output = IndexMap<OwnedUserId, Receipt>> + SendOutsideWasm + 'a;

    /// Load the current fully-read event id, from storage.
    fn load_fully_read_marker(&self) -> impl Future<Output = Option<OwnedEventId>> + '_;

    fn push_context(&self) -> impl Future<Output = Option<PushContext>> + SendOutsideWasm + '_;

    /// Send an event to that room.
    fn send(
        &self,
        content: AnyMessageLikeEventContent,
    ) -> impl Future<Output = Result<(), super::Error>> + SendOutsideWasm + '_;

    /// Redact an event from that room.
    fn redact<'a>(
        &'a self,
        event_id: &'a EventId,
        reason: Option<&'a str>,
        transaction_id: Option<OwnedTransactionId>,
    ) -> impl Future<Output = Result<(), super::Error>> + SendOutsideWasm + 'a;

    fn room_info(&self) -> Subscriber<RoomInfo>;

    /// Return the encryption info for the Megolm session with the supplied
    /// session ID.
    fn get_encryption_info(
        &self,
        session_id: &str,
        sender: &UserId,
    ) -> impl Future<Output = Option<Arc<EncryptionInfo>>> + SendOutsideWasm;

    async fn relations(&self, event_id: OwnedEventId, opts: RelationsOptions) -> Result<Relations>;

    /// Loads an event from the cache or network.
    fn load_event<'a>(
        &'a self,
        event_id: &'a EventId,
    ) -> impl Future<Output = Result<TimelineEvent>> + SendOutsideWasm + 'a;
}

impl RoomDataProvider for Room {
    fn own_user_id(&self) -> &UserId {
        (**self).own_user_id()
    }

    fn room_version(&self) -> RoomVersionId {
        (**self).clone_info().room_version_or_default()
    }

    async fn crypto_context_info(&self) -> CryptoContextInfo {
        self.crypto_context_info().await
    }

    async fn profile_from_user_id<'a>(&'a self, user_id: &'a UserId) -> Option<Profile> {
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

    fn profile_from_latest_event(&self, latest_event: &LatestEvent) -> Option<Profile> {
        if !latest_event.has_sender_profile() {
            return None;
        }

        Some(Profile {
            display_name: latest_event.sender_display_name().map(ToOwned::to_owned),
            display_name_ambiguous: latest_event.sender_name_ambiguous().unwrap_or(false),
            avatar_url: latest_event.sender_avatar_url().map(ToOwned::to_owned),
        })
    }

    async fn load_user_receipt<'a>(
        &'a self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &'a UserId,
    ) -> Option<(OwnedEventId, Receipt)> {
        match self.load_user_receipt(receipt_type.clone(), thread.clone(), user_id).await {
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

    async fn load_event_receipts<'a>(
        &'a self,
        event_id: &'a EventId,
    ) -> IndexMap<OwnedUserId, Receipt> {
        let mut unthreaded_receipts = match self
            .load_event_receipts(ReceiptType::Read, ReceiptThread::Unthreaded, event_id)
            .await
        {
            Ok(receipts) => receipts.into_iter().collect(),
            Err(e) => {
                error!(?event_id, "Failed to get unthreaded read receipts for event: {e}");
                IndexMap::new()
            }
        };

        let main_thread_receipts = match self
            .load_event_receipts(ReceiptType::Read, ReceiptThread::Main, event_id)
            .await
        {
            Ok(receipts) => receipts,
            Err(e) => {
                error!(?event_id, "Failed to get main thread read receipts for event: {e}");
                Vec::new()
            }
        };

        unthreaded_receipts.extend(main_thread_receipts);
        unthreaded_receipts
    }

    async fn push_context(&self) -> Option<PushContext> {
        self.push_context().await.ok().flatten()
    }

    async fn load_fully_read_marker(&self) -> Option<OwnedEventId> {
        match self.account_data_static::<FullyReadEventContent>().await {
            Ok(Some(fully_read)) => match fully_read.deserialize() {
                Ok(fully_read) => Some(fully_read.content.event_id),
                Err(e) => {
                    error!("Failed to deserialize fully-read account data: {e}");
                    None
                }
            },
            Err(e) => {
                error!("Failed to get fully-read account data from the store: {e}");
                None
            }
            _ => None,
        }
    }

    async fn send(&self, content: AnyMessageLikeEventContent) -> Result<(), super::Error> {
        let _ = self.send_queue().send(content).await?;
        Ok(())
    }

    async fn redact<'a>(
        &'a self,
        event_id: &'a EventId,
        reason: Option<&'a str>,
        transaction_id: Option<OwnedTransactionId>,
    ) -> Result<(), super::Error> {
        let _ = self
            .redact(event_id, reason, transaction_id)
            .await
            .map_err(RedactError::HttpError)
            .map_err(super::Error::RedactError)?;
        Ok(())
    }

    fn room_info(&self) -> Subscriber<RoomInfo> {
        self.subscribe_info()
    }

    async fn get_encryption_info(
        &self,
        session_id: &str,
        sender: &UserId,
    ) -> Option<Arc<EncryptionInfo>> {
        // Pass directly on to `Room::get_encryption_info`
        self.get_encryption_info(session_id, sender).await
    }

    async fn relations(&self, event_id: OwnedEventId, opts: RelationsOptions) -> Result<Relations> {
        self.relations(event_id, opts).await
    }

    async fn load_event<'a>(&'a self, event_id: &'a EventId) -> Result<TimelineEvent> {
        self.load_or_fetch_event(event_id, None).await
    }
}

// Internal helper to make most of retry_event_decryption independent of a room
// object, which is annoying to create for testing and not really needed
pub(super) trait Decryptor: AsyncTraitDeps + Clone + 'static {
    fn decrypt_event_impl(
        &self,
        raw: &Raw<AnySyncTimelineEvent>,
        push_ctx: Option<&PushContext>,
    ) -> impl Future<Output = Result<TimelineEvent>> + SendOutsideWasm;
}

impl Decryptor for Room {
    async fn decrypt_event_impl(
        &self,
        raw: &Raw<AnySyncTimelineEvent>,
        push_ctx: Option<&PushContext>,
    ) -> Result<TimelineEvent> {
        self.decrypt_event(raw.cast_ref(), push_ctx).await
    }
}

#[cfg(test)]
impl Decryptor for (matrix_sdk_base::crypto::OlmMachine, ruma::OwnedRoomId) {
    async fn decrypt_event_impl(
        &self,
        raw: &Raw<AnySyncTimelineEvent>,
        push_ctx: Option<&PushContext>,
    ) -> Result<TimelineEvent> {
        let (olm_machine, room_id) = self;
        let decryption_settings =
            DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

        match olm_machine
            .try_decrypt_room_event(raw.cast_ref(), room_id, &decryption_settings)
            .await?
        {
            RoomEventDecryptionResult::Decrypted(decrypted) => {
                let push_actions = push_ctx.map(|push_ctx| push_ctx.for_event(&decrypted.event));
                Ok(TimelineEvent::from_decrypted(decrypted, push_actions))
            }
            RoomEventDecryptionResult::UnableToDecrypt(utd_info) => {
                Ok(TimelineEvent::from_utd(raw.clone(), utd_info))
            }
        }
    }
}
