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
// See the License for that specific language governing permissions and
// limitations under the License.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::{future::ready, pin_mut, StreamExt as _};
use matrix_sdk::{room::Room, Client};
use matrix_sdk_base::{deserialized_responses::TimelineEvent, StoreError};
use ruma::{
    api::client::sync::sync_events::v4::{AccountDataConfig, RoomSubscription},
    assign,
    events::{AnySyncTimelineEvent, StateEventType},
    push::Action,
    serde::Raw,
    uint, EventId, RoomId,
};
use thiserror::Error;

use crate::encryption_sync::{EncryptionSync, WithLocking};

/// A client specialized for handling push notifications received over the
/// network, for an app.
///
/// In particular, it takes care of running a full decryption sync, in case the
/// event in the notification was impossible to decrypt beforehand.
pub struct NotificationClient {
    /// SDK client.
    client: Client,

    /// Should we retry decrypting an event, after it was impossible to decrypt
    /// on the first attempt?
    retry_decryption: bool,

    /// Should the encryption sync happening in case the notification event was
    /// encrypted use a cross-process lock?
    ///
    /// Only meaningful if `retry_decryption` is true.
    with_cross_process_lock: bool,

    /// Should we try to filter out the notification event according to the push
    /// rules?
    filter_by_push_rules: bool,
}

impl NotificationClient {
    const CONNECTION_ID: &str = "notifications";
    const LOCK_ID: &str = "notifications";

    /// Create a new builder for a notification client.
    pub fn builder(client: Client) -> NotificationClientBuilder {
        NotificationClientBuilder::new(client)
    }

    /// Run an encryption sync loop, in case an event is still encrypted.
    ///
    /// Will return true if and only:
    /// - retry_decryption was enabled,
    /// - the event was encrypted,
    /// - we successfully ran an encryption sync.
    async fn maybe_retry_decryption(
        &self,
        raw_event: &Raw<AnySyncTimelineEvent>,
    ) -> Result<bool, Error> {
        if !self.retry_decryption {
            return Ok(false);
        }

        let event: AnySyncTimelineEvent =
            raw_event.deserialize().map_err(|_| Error::InvalidRumaEvent)?;

        let event_type = event.event_type();

        let is_still_encrypted =
            matches!(event_type, ruma::events::TimelineEventType::RoomEncrypted);

        #[cfg(feature = "unstable-msc3956")]
        let is_still_encrypted =
            is_still_encrypted || matches!(event_type, ruma::events::TimelineEventType::Encrypted);

        if !is_still_encrypted {
            return Ok(false);
        }

        // The message is still encrypted, and the client is configured to retry
        // decryption.
        //
        // Spawn an `EncryptionSync` that runs two iterations of the sliding sync loop:
        // - the first iteration allows to get SS events as well as send e2ee requests.
        // - the second one let the SS proxy forward events triggered by the sending of
        // e2ee requests.
        //
        // Keep timeouts small for both, since we might be short on time.

        let with_locking = WithLocking::from(self.with_cross_process_lock);

        let encryption_sync = EncryptionSync::new(
            Self::LOCK_ID.to_owned(),
            self.client.clone(),
            Some((Duration::from_secs(3), Duration::from_secs(1))),
            with_locking,
        )
        .await;

        // Just log out errors, but don't have them abort the notification processing:
        // an undecrypted notification is still better than no
        // notifications.

        match encryption_sync {
            Ok(sync) => match sync.run_fixed_iterations(2).await {
                Ok(()) => Ok(true),
                Err(err) => {
                    tracing::warn!(
                        "error when running encryption_sync in get_notification: {err:#}"
                    );
                    Ok(false)
                }
            },
            Err(err) => {
                tracing::warn!("error when building encryption_sync in get_notification: {err:#}",);
                Ok(false)
            }
        }
    }

    /// Try to run a sliding sync (without encryption) to retrieve the event
    /// from the notification.
    ///
    /// This works by requesting explicit state that'll be useful for building
    /// the `NotificationItem`, and subscribing to the room which the
    /// notification relates to.
    async fn try_sliding_sync(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<Raw<AnySyncTimelineEvent>>, Error> {
        // Set up a sliding sync that only subscribes to the room that had the
        // notification, so we can figure out the full event and associated
        // information.

        let notification = Arc::new(Mutex::new(None));

        let cloned_notif = notification.clone();
        let event_id = event_id.to_owned();

        let event_handler = self.client.add_event_handler(move |raw: Raw<AnySyncTimelineEvent>| {
            match raw.deserialize() {
                Ok(ev) => {
                    if ev.event_id() == event_id {
                        // found it! There shouldn't be a previous event before, but if there is,
                        // that should be ok to just replace it.
                        *cloned_notif.lock().unwrap() = Some(raw);
                    }
                }
                Err(err) => {
                    tracing::warn!("could not deserialize AnySyncTimelineEvent: {err:#}");
                }
            }
            ready(())
        });

        let sync = self
            .client
            .sliding_sync(Self::CONNECTION_ID)?
            .with_timeouts(Duration::from_secs(3), Duration::from_secs(1))
            .with_account_data_extension(
                assign!(AccountDataConfig::default(), { enabled: Some(true) }),
            )
            .build()
            .await?;

        sync.subscribe_to_room(
            room_id.to_owned(),
            Some(assign!(RoomSubscription::default(), {
                required_state: vec![
                    (StateEventType::RoomAvatar, "".to_owned()),
                    (StateEventType::RoomEncryption, "".to_owned()),
                    (StateEventType::RoomMember, "$LAZY".to_owned()),
                    (StateEventType::RoomCanonicalAlias, "".to_owned()),
                    (StateEventType::RoomName, "".to_owned()),
                    (StateEventType::RoomPowerLevels, "".to_owned()), // necessary to build the push context
                ],
                timeline_limit: Some(uint!(16))
            })),
        );

        let update_summary = {
            let stream = sync.sync();
            pin_mut!(stream);
            if let Some(result) = stream.next().await {
                result?
            } else {
                // Sliding sync aborted early.
                return Ok(None);
            }
        };

        self.client.remove_event_handler(event_handler);

        if !update_summary.lists.is_empty() {
            tracing::warn!(
                "unexpected, non-empty list of lists in the notification (non e2ee) sliding sync: {:?}",
                update_summary.lists
            );
        }

        if update_summary.rooms.is_empty() {
            return Err(Error::SlidingSyncEmptyRoom);
        }

        let maybe_event = notification.lock().unwrap().take();
        Ok(maybe_event)
    }

    /// Get a full notification, given a room id and event id.
    ///
    /// This will run a small sliding sync to retrieve the content of the event,
    /// along with extra data to form a rich notification context.
    ///
    /// For this to work, this sliding sync must not interfere with other
    /// sliding syncs running for the same client. If this is running in a
    /// separate process, then the state store must be independent from
    /// those used in other processes, at the risk of corrupting in-memory
    /// caches. Ideally, an in-memory store could and should be used in that
    /// context.
    pub async fn get_notification_with_sliding_sync(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<NotificationItem>, Error> {
        tracing::info!("fetching notification event with a sliding sync");

        let mut raw_event = match self.try_sliding_sync(room_id, event_id).await {
            Ok(Some(raw_event)) => raw_event,

            Ok(None) => {
                tracing::debug!("notification sync hasn't found the event");
                return Ok(None);
            }

            Err(err) => {
                tracing::warn!("notification sync has run into an error: {err:#}");
                return Ok(None);
            }
        };

        // At this point it should have been added by the sync, if it's not, give up.
        let Some(room) = self.client.get_room(room_id) else { return Err(Error::UnknownRoom) };

        if self.maybe_retry_decryption(&raw_event).await? {
            // TODO technically, we could just retry decryption of the previous event?
            match self.try_sliding_sync(room_id, event_id).await {
                Ok(Some(new_raw_event)) => {
                    raw_event = new_raw_event;
                }
                Ok(None) => {
                    // The event was there, but disappeared in the next sync; maybe there
                    // were too many events happening in between?
                    tracing::warn!("event missing after retrying decryption in get_notification?");
                }
                Err(err) => {
                    tracing::warn!("error when retrying decryption in get_notification: {err:#}");
                }
            }
        }

        let push_actions = room.event_push_actions(&raw_event).await?;
        if let Some(push_actions) = &push_actions {
            if self.filter_by_push_rules && !push_actions.iter().any(|a| a.should_notify()) {
                return Ok(None);
            }
        }

        Ok(Some(NotificationItem::new(false, &room, &raw_event, push_actions.as_deref()).await?))
    }

    async fn handle_timeline_event(
        &self,
        room: &Room,
        mut timeline_event: TimelineEvent,
        sync_members: bool,
    ) -> Result<Option<NotificationItem>, Error> {
        if self.maybe_retry_decryption(timeline_event.event.cast_ref()).await? {
            match room.decrypt_event(timeline_event.event.cast_ref()).await {
                Ok(event) => {
                    timeline_event = event;
                }
                Err(err) => {
                    tracing::warn!("error when retrying decryption in get_notification: {err:#}");
                }
            }
        }

        if self.filter_by_push_rules
            && !timeline_event.push_actions.iter().any(|a| a.should_notify())
        {
            return Ok(None);
        }

        Ok(Some(
            NotificationItem::new(
                sync_members,
                room,
                timeline_event.event.cast_ref(),
                Some(&timeline_event.push_actions),
            )
            .await?,
        ))
    }

    /// Retrieve a notification using a /context query.
    ///
    /// This is for clients that are already running other sliding syncs in the
    /// same process, so that most of the contextual information for the
    /// notification should already be there.
    pub async fn get_notification_with_context(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<NotificationItem>, Error> {
        tracing::info!("fetching notification event with a /context query");

        // XXX Will this work for invites? A sliding sync may be running in the
        // background, but it's unclear that it's fetched the invite event yet.
        // bnjbvr thinks this is racy.
        let Some(room) = self.client.get_room(room_id) else { return Err(Error::UnknownRoom) };

        let timeline_event =
            room.event_with_context(event_id, true).await?.ok_or(Error::ContextMissingEvent)?;

        // No need to sync members, the /context query should have fetched the member.
        self.handle_timeline_event(&room, timeline_event, false).await
    }

    /// Legacy implementation to retrieve the content of a notification.
    ///
    /// This doesn't work for invites, and may miss extra context that the
    /// notification wants. It will likely be removed in subsequent commits.
    pub async fn legacy_get_notification(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<NotificationItem>, Error> {
        tracing::info!("legacy attempt at fetching the notification");

        // XXX This won't work for invites.
        let Some(room) = self.client.get_room(room_id) else { return Err(Error::UnknownRoom) };

        let timeline_event = room.event(event_id).await?;

        self.handle_timeline_event(&room, timeline_event, true).await
    }
}

/// Builder for a `NotificationClient`.
///
/// Fields have the same meaning as in `NotificationClient`.
#[derive(Clone)]
pub struct NotificationClientBuilder {
    client: Client,
    retry_decryption: bool,
    with_cross_process_lock: bool,
    filter_by_push_rules: bool,
}

impl NotificationClientBuilder {
    fn new(client: Client) -> Self {
        Self {
            client,
            retry_decryption: false,
            with_cross_process_lock: false,
            filter_by_push_rules: false,
        }
    }

    /// Filter out the notification event according to the push rules present in
    /// the event.
    pub fn filter_by_push_rules(mut self) -> Self {
        self.filter_by_push_rules = true;
        self
    }

    /// Automatically retry decryption once, if the notification was received
    /// encrypted.
    ///
    /// The boolean indicates whether we're making use of a cross-process lock
    /// for the crypto-store. This should be set to true, if and only if,
    /// the notification is received in a process that's different from the
    /// main app.
    pub fn retry_decryption(mut self, with_cross_process_lock: bool) -> Self {
        self.retry_decryption = true;
        self.with_cross_process_lock = with_cross_process_lock;
        self
    }

    /// Finishes configuring the `NotificationClient`.
    pub fn build(self) -> NotificationClient {
        NotificationClient {
            client: self.client,
            with_cross_process_lock: self.with_cross_process_lock,
            filter_by_push_rules: self.filter_by_push_rules,
            retry_decryption: self.retry_decryption,
        }
    }
}

/// A notification with its full content.
pub struct NotificationItem {
    /// Underlying Ruma event.
    pub event: AnySyncTimelineEvent,

    /// Display name of the sender.
    pub sender_display_name: Option<String>,
    /// Avatar URL of the sender.
    pub sender_avatar_url: Option<String>,

    /// Room display name.
    pub room_display_name: String,
    /// Room avatar URL.
    pub room_avatar_url: Option<String>,
    /// Room canonical alias.
    pub room_canonical_alias: Option<String>,
    /// Is this room encrypted?
    pub is_room_encrypted: Option<bool>,
    /// Is this room considered a direct message?
    pub is_direct_message_room: bool,
    /// Numbers of members who joined the room.
    pub joined_members_count: u64,

    /// Is it a noisy notification? (i.e. does any push action contain a sound
    /// action)
    ///
    /// It is set if and only if the push actions could be determined.
    pub is_noisy: Option<bool>,
}

impl NotificationItem {
    async fn new(
        sync_members: bool,
        room: &Room,
        event: &Raw<AnySyncTimelineEvent>,
        push_actions: Option<&[Action]>,
    ) -> Result<Self, Error> {
        let raw_event = event.deserialize().map_err(|_| Error::InvalidRumaEvent)?;

        let sender = match &room {
            Room::Invited(invited) => invited.invite_details().await?.inviter,
            _ => {
                if sync_members {
                    room.get_member(raw_event.sender()).await?
                } else {
                    room.get_member_no_sync(raw_event.sender()).await?
                }
            }
        };

        let (sender_display_name, sender_avatar_url) = match sender {
            Some(sender) => (
                sender.display_name().map(|s| s.to_owned()),
                sender.avatar_url().map(|s| s.to_string()),
            ),
            None => (None, None),
        };

        let is_noisy = push_actions.map(|actions| actions.iter().any(|a| a.sound().is_some()));

        let item = NotificationItem {
            event: raw_event,
            sender_display_name,
            sender_avatar_url,
            room_display_name: room.display_name().await?.to_string(),
            room_avatar_url: room.avatar_url().map(|s| s.to_string()),
            room_canonical_alias: room.canonical_alias().map(|c| c.to_string()),
            is_direct_message_room: room.is_direct().await?,
            is_room_encrypted: room.is_encrypted().await.ok(),
            joined_members_count: room.joined_members_count(),
            is_noisy,
        };

        Ok(item)
    }
}

/// An error for the [`NotificationClient`].
#[derive(Debug, Error)]
pub enum Error {
    /// The room associated to this event wasn't found.
    #[error("unknown room for a notification")]
    UnknownRoom,

    /// The Ruma event contained within this notification couldn't be parsed.
    #[error("invalid ruma event")]
    InvalidRumaEvent,

    /// When calling `get_notification_with_sliding_sync`, the room was missing
    /// in the response.
    #[error("the sliding sync response doesn't include the target room")]
    SlidingSyncEmptyRoom,

    #[error("the event was missing in the /context query")]
    ContextMissingEvent,

    /// An error forwarded from the client.
    #[error(transparent)]
    SdkError(#[from] matrix_sdk::Error),

    /// An error forwarded from the underlying state store.
    #[error(transparent)]
    StoreError(#[from] StoreError),
}
