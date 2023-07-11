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

use std::time::Duration;

use matrix_sdk::{room::Room, Client};
use matrix_sdk_base::StoreError;
use ruma::{events::AnySyncTimelineEvent, EventId, RoomId};
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
    /// Create a new builder for a notification client.
    pub fn builder(client: Client) -> NotificationClientBuilder {
        NotificationClientBuilder::new(client)
    }

    /// Get a full notification, given a room id and event id.
    pub async fn get_notification(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<NotificationItem>, Error> {
        // TODO(bnjbvr) invites don't have access to the room! Make a separate path to
        // handle those?
        let Some(room) = self.client.get_room(room_id) else { return Err(Error::UnknownRoom) };

        let mut timeline_event = room.event(event_id).await?;

        if self.filter_by_push_rules
            && timeline_event
                .push_actions
                .as_ref()
                .is_some_and(|push_actions| !push_actions.iter().any(|a| a.should_notify()))
        {
            return Ok(None);
        }

        let mut raw_event: AnySyncTimelineEvent =
            timeline_event.event.deserialize().map_err(|_| Error::InvalidRumaEvent)?.into();

        let event_type = raw_event.event_type();

        let is_still_encrypted =
            matches!(event_type, ruma::events::TimelineEventType::RoomEncrypted);

        #[cfg(feature = "unstable-msc3956")]
        let is_still_encrypted =
            is_still_encrypted || matches!(event_type, ruma::events::TimelineEventType::Encrypted);

        if is_still_encrypted && self.retry_decryption {
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
                "notifications".to_owned(),
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
                    Ok(()) => match room.decrypt_event(timeline_event.event.cast_ref()).await {
                        Ok(decrypted_event) => {
                            timeline_event = decrypted_event;
                            raw_event = timeline_event
                                .event
                                .deserialize()
                                .map_err(|_| Error::InvalidRumaEvent)?
                                .into();
                        }
                        Err(err) => {
                            tracing::warn!(
                                "error when retrying decryption in get_notification: {err:#}"
                            );
                        }
                    },
                    Err(err) => {
                        tracing::warn!(
                            "error when running encryption_sync in get_notification: {err:#}"
                        );
                    }
                },
                Err(err) => {
                    tracing::warn!(
                        "error when building encryption_sync in get_notification: {err:#}",
                    );
                }
            }
        }

        let sender = match &room {
            Room::Invited(invited) => invited.invite_details().await?.inviter,
            _ => room.get_member(raw_event.sender()).await?,
        };

        let (sender_display_name, sender_avatar_url) = match sender {
            Some(sender) => (
                sender.display_name().map(|s| s.to_owned()),
                sender.avatar_url().map(|s| s.to_string()),
            ),
            None => (None, None),
        };

        let is_noisy = timeline_event
            .push_actions
            .as_ref()
            .is_some_and(|push_actions| push_actions.iter().any(|a| a.sound().is_some()));

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

        Ok(Some(item))
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
            with_cross_process_lock: false,
            filter_by_push_rules: false,
            retry_decryption: false,
            client,
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
    pub is_noisy: bool,
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

    /// An error forwarded from the client.
    #[error(transparent)]
    SdkError(#[from] matrix_sdk::Error),

    /// An error forwarded from the underlying state store.
    #[error(transparent)]
    StoreError(#[from] StoreError),
}
