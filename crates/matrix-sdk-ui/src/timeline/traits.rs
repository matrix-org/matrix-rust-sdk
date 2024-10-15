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

use std::future::Future;

use eyeball::Subscriber;
use indexmap::IndexMap;
#[cfg(test)]
use matrix_sdk::crypto::{DecryptionSettings, TrustRequirement};
use matrix_sdk::{
    deserialized_responses::TimelineEvent,
    event_cache::paginator::PaginableRoom,
    executor::{BoxFuture, BoxFutureExt as _},
    Result, Room, SendOutsideWasm, SyncOutsideWasm,
};
use matrix_sdk_base::{latest_event::LatestEvent, RoomInfo};
use ruma::{
    events::{
        fully_read::FullyReadEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        AnyMessageLikeEventContent, AnySyncTimelineEvent,
    },
    push::{PushConditionRoomCtx, Ruleset},
    serde::Raw,
    EventId, OwnedEventId, OwnedTransactionId, OwnedUserId, RoomVersionId, UserId,
};
use tracing::{debug, error};

use super::{Profile, RedactError, TimelineBuilder};
use crate::timeline::{self, pinned_events_loader::PinnedEventsRoom, Timeline};

pub trait RoomExt {
    /// Get a [`Timeline`] for this room.
    ///
    /// This offers a higher-level API than event handlers, in treating things
    /// like edits and reactions as updates of existing items rather than new
    /// independent events.
    ///
    /// This is the same as using `room.timeline_builder().build()`.
    #[cfg(not(target_arch = "wasm32"))]
    fn timeline(&self) -> impl Future<Output = Result<Timeline, timeline::Error>> + Send;

    /// Get a [`Timeline`] for this room.
    ///
    /// This offers a higher-level API than event handlers, in treating things
    /// like edits and reactions as updates of existing items rather than new
    /// independent events.
    ///
    /// This is the same as using `room.timeline_builder().build()`.
    #[cfg(target_arch = "wasm32")]
    fn timeline(&self) -> impl Future<Output = Result<Timeline, timeline::Error>>;

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

impl RoomExt for Room {
    async fn timeline(&self) -> Result<Timeline, timeline::Error> {
        self.timeline_builder().build().await
    }

    fn timeline_builder(&self) -> TimelineBuilder {
        Timeline::builder(self).track_read_marker_and_receipts()
    }
}

pub(super) trait RoomDataProvider:
    Clone + SendOutsideWasm + SyncOutsideWasm + 'static + PaginableRoom + PinnedEventsRoom
{
    fn own_user_id(&self) -> &UserId;
    fn room_version(&self) -> RoomVersionId;

    fn profile_from_user_id<'a>(&'a self, user_id: &'a UserId) -> BoxFuture<'a, Option<Profile>>;
    fn profile_from_latest_event(&self, latest_event: &LatestEvent) -> Option<Profile>;

    /// Loads a user receipt from the storage backend.
    fn load_user_receipt<'a>(
        &'a self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &'a UserId,
    ) -> BoxFuture<'a, Option<(OwnedEventId, Receipt)>>;

    /// Loads read receipts for an event from the storage backend.
    fn load_event_receipts<'a>(
        &'a self,
        event_id: &'a EventId,
    ) -> BoxFuture<'a, IndexMap<OwnedUserId, Receipt>>;

    /// Load the current fully-read event id, from storage.
    fn load_fully_read_marker(&self) -> BoxFuture<'_, Option<OwnedEventId>>;

    fn push_rules_and_context(&self) -> BoxFuture<'_, Option<(Ruleset, PushConditionRoomCtx)>>;

    /// Send an event to that room.
    fn send(&self, content: AnyMessageLikeEventContent) -> BoxFuture<'_, Result<(), super::Error>>;

    /// Redact an event from that room.
    fn redact<'a>(
        &'a self,
        event_id: &'a EventId,
        reason: Option<&'a str>,
        transaction_id: Option<OwnedTransactionId>,
    ) -> BoxFuture<'a, Result<(), super::Error>>;

    fn room_info(&self) -> Subscriber<RoomInfo>;
}

impl RoomDataProvider for Room {
    fn own_user_id(&self) -> &UserId {
        (**self).own_user_id()
    }

    fn room_version(&self) -> RoomVersionId {
        (**self).clone_info().room_version_or_default()
    }

    fn profile_from_user_id<'a>(&'a self, user_id: &'a UserId) -> BoxFuture<'a, Option<Profile>> {
        async move {
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
        .box_future()
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

    fn load_user_receipt<'a>(
        &'a self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &'a UserId,
    ) -> BoxFuture<'a, Option<(OwnedEventId, Receipt)>> {
        async move {
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
        .box_future()
    }

    fn load_event_receipts<'a>(
        &'a self,
        event_id: &'a EventId,
    ) -> BoxFuture<'a, IndexMap<OwnedUserId, Receipt>> {
        async move {
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
        .box_future()
    }

    fn push_rules_and_context(&self) -> BoxFuture<'_, Option<(Ruleset, PushConditionRoomCtx)>> {
        async {
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
        .box_future()
    }

    fn load_fully_read_marker(&self) -> BoxFuture<'_, Option<OwnedEventId>> {
        async {
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
        .box_future()
    }

    fn send(&self, content: AnyMessageLikeEventContent) -> BoxFuture<'_, Result<(), super::Error>> {
        async move {
            let _ = self.send_queue().send(content).await?;
            Ok(())
        }
        .box_future()
    }

    fn redact<'a>(
        &'a self,
        event_id: &'a EventId,
        reason: Option<&'a str>,
        transaction_id: Option<OwnedTransactionId>,
    ) -> BoxFuture<'a, Result<(), super::Error>> {
        async move {
            let _ = self
                .redact(event_id, reason, transaction_id)
                .await
                .map_err(RedactError::HttpError)
                .map_err(super::Error::RedactError)?;
            Ok(())
        }
        .box_future()
    }

    fn room_info(&self) -> Subscriber<RoomInfo> {
        self.subscribe_info()
    }
}

// Internal helper to make most of retry_event_decryption independent of a room
// object, which is annoying to create for testing and not really needed
pub(super) trait Decryptor: Clone + SendOutsideWasm + SyncOutsideWasm + 'static {
    #[cfg(not(target_arch = "wasm32"))]
    fn decrypt_event_impl(
        &self,
        raw: &Raw<AnySyncTimelineEvent>,
    ) -> impl Future<Output = Result<TimelineEvent>> + Send;

    #[cfg(target_arch = "wasm32")]
    fn decrypt_event_impl(
        &self,
        raw: &Raw<AnySyncTimelineEvent>,
    ) -> impl Future<Output = Result<TimelineEvent>>;
}

impl Decryptor for Room {
    async fn decrypt_event_impl(&self, raw: &Raw<AnySyncTimelineEvent>) -> Result<TimelineEvent> {
        self.decrypt_event(raw.cast_ref()).await
    }
}

#[cfg(test)]
impl Decryptor for (matrix_sdk_base::crypto::OlmMachine, ruma::OwnedRoomId) {
    async fn decrypt_event_impl(&self, raw: &Raw<AnySyncTimelineEvent>) -> Result<TimelineEvent> {
        let (olm_machine, room_id) = self;
        let decryption_settings =
            DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };
        let event =
            olm_machine.decrypt_room_event(raw.cast_ref(), room_id, &decryption_settings).await?;
        Ok(event.into())
    }
}
