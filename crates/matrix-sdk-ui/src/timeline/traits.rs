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

use std::future::Future;

use eyeball::Subscriber;
use indexmap::IndexMap;
use matrix_sdk::{
    Result, Room, SendOutsideWasm,
    deserialized_responses::TimelineEvent,
    paginators::{PaginableRoom, thread::PaginableThread},
};
use matrix_sdk_base::{RoomInfo, crypto::types::events::CryptoContextInfo};
use ruma::{
    EventId, OwnedEventId, OwnedTransactionId, OwnedUserId, UserId,
    events::{
        AnyMessageLikeEventContent,
        fully_read::FullyReadEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
    },
    room_version_rules::RoomVersionRules,
};
use tracing::error;

use super::{Profile, RedactError, TimelineBuilder};
use crate::timeline::{
    self, Timeline, TimelineReadReceiptTracking, latest_event::LatestEventValue,
    pinned_events_loader::PinnedEventsRoom,
};

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

    /// Return a [`LatestEventValue`] corresponding to this room's latest event.
    fn latest_event(&self) -> impl Future<Output = LatestEventValue>;
}

impl RoomExt for Room {
    async fn timeline(&self) -> Result<Timeline, timeline::Error> {
        self.timeline_builder().build().await
    }

    fn timeline_builder(&self) -> TimelineBuilder {
        TimelineBuilder::new(self)
            .track_read_marker_and_receipts(TimelineReadReceiptTracking::AllEvents)
    }

    async fn latest_event(&self) -> LatestEventValue {
        LatestEventValue::from_base_latest_event_value(
            (**self).latest_event(),
            self,
            &self.client(),
        )
        .await
    }
}

pub(super) trait RoomDataProvider:
    Clone + PaginableRoom + PaginableThread + PinnedEventsRoom + 'static
{
    fn own_user_id(&self) -> &UserId;
    fn room_version_rules(&self) -> RoomVersionRules;

    fn crypto_context_info(&self)
    -> impl Future<Output = CryptoContextInfo> + SendOutsideWasm + '_;

    fn profile_from_user_id<'a>(
        &'a self,
        user_id: &'a UserId,
    ) -> impl Future<Output = Option<Profile>> + SendOutsideWasm + 'a;

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
        receipt_thread: ReceiptThread,
    ) -> impl Future<Output = IndexMap<OwnedUserId, Receipt>> + SendOutsideWasm + 'a;

    /// Load the current fully-read event id, from storage.
    fn load_fully_read_marker(&self) -> impl Future<Output = Option<OwnedEventId>> + '_;

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

    fn room_version_rules(&self) -> RoomVersionRules {
        (**self).clone_info().room_version_rules_or_default()
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
        receipt_thread: ReceiptThread,
    ) -> IndexMap<OwnedUserId, Receipt> {
        match self.load_event_receipts(ReceiptType::Read, receipt_thread.clone(), event_id).await {
            Ok(receipts) => receipts.into_iter().collect(),
            Err(e) => {
                error!(?event_id, ?receipt_thread, "Failed to get read receipts for event: {e}");
                IndexMap::new()
            }
        }
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

    async fn load_event<'a>(&'a self, event_id: &'a EventId) -> Result<TimelineEvent> {
        self.load_or_fetch_event(event_id, None).await
    }
}
