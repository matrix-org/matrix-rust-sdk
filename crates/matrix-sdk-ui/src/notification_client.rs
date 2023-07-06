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

use std::{sync::Arc, time::Duration};

use matrix_sdk::{room::Room, Client};
use matrix_sdk_base::StoreError;
use ruma::{events::AnySyncTimelineEvent, EventId, RoomId};
use thiserror::Error;

use crate::encryption_sync::{self, EncryptionSync, WithLocking};

pub struct NotificationClient {
    client: Client,
    with_cross_process_lock: bool,
    retry_decryption: bool,
    filter_by_push_rules: bool,
}

impl NotificationClient {
    pub fn builder(client: Client) -> NotificationClientBuilder {
        NotificationClientBuilder::new(client)
    }

    pub async fn get_notification_item(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<NotificationItem>, Error> {
        let Some(room) = self.client.get_room(&room_id) else { return Err(Error::UnknownRoom) };

        let mut retry_decryption = self.retry_decryption;

        let (ruma_event, event) = loop {
            let ruma_event = room.event(&event_id).await?;
            if self.filter_by_push_rules
                && !ruma_event.push_actions.iter().any(|a| a.should_notify())
            {
                return Ok(None);
            }

            let event: AnySyncTimelineEvent =
                ruma_event.event.deserialize().map_err(|_| Error::InvalidRumaEvent)?.into();

            let event_type = event.event_type();

            let is_still_encrypted =
                matches!(event_type, ruma::events::TimelineEventType::RoomEncrypted);

            #[cfg(feature = "unstable-msc3956")]
            let is_still_encrypted = is_still_encrypted
                || matches!(event_type, ruma::events::TimelineEventType::Encrypted);

            if is_still_encrypted && retry_decryption {
                // The message is still encrypted, and the client is configured to retry
                // decryption.
                //
                // Spawn an `EncryptionSync` that runs two iterations of the sliding sync loop:
                // - the first iteration allows to get SS events as well as send e2ee requests.
                // - the second one let the SS proxy forward events triggered by the sending of
                // e2ee requests.
                //
                // Keep timeouts small for both, since we might be short on time.

                // Don't iloop retrying decryption another time.
                retry_decryption = false;

                let with_locking = WithLocking::from(self.with_cross_process_lock);

                let encryption_sync = EncryptionSync::new(
                    "notifications".to_owned(),
                    self.client.clone(),
                    Some((Duration::from_secs(3), Duration::from_secs(1))),
                    with_locking,
                )
                .await;

                if let Ok(sync) = encryption_sync {
                    if sync.run_fixed_iterations(2).await.is_ok() {
                        continue;
                    }
                }
            }

            break (ruma_event, event);
        };

        let sender = match &room {
            Room::Invited(invited) => invited.invite_details().await?.inviter,
            _ => room.get_member(event.sender()).await?,
        };
        let mut sender_display_name = None;
        let mut sender_avatar_url = None;
        if let Some(sender) = sender {
            sender_display_name = sender.display_name().map(|s| s.to_owned());
            sender_avatar_url = sender.avatar_url().map(|s| s.to_string());
        }

        let is_noisy = ruma_event.push_actions.iter().any(|a| a.sound().is_some());

        let item = NotificationItem {
            event: Arc::new(event),
            room_id: room.room_id().to_string(),
            sender_display_name,
            sender_avatar_url,
            room_display_name: room.display_name().await?.to_string(),
            room_avatar_url: room.avatar_url().map(|s| s.to_string()),
            room_canonical_alias: room.canonical_alias().map(|c| c.to_string()),
            is_noisy,
            is_direct: room.is_direct().await?,
            is_room_encrypted: room.is_encrypted().await.ok(),
        };

        Ok(Some(item))
    }
}

pub struct NotificationClientBuilder {
    with_cross_process_lock: bool,
    filter_by_push_rules: bool,
    retry_decryption: bool,
    client: Client,
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

    pub fn with_cross_process_lock(mut self) -> Self {
        self.with_cross_process_lock = true;
        self
    }

    pub fn filter_by_push_rules(mut self) -> Self {
        self.filter_by_push_rules = true;
        self
    }

    pub fn retry_decryption(mut self) -> Self {
        self.retry_decryption = true;
        self
    }

    pub fn build(self) -> NotificationClient {
        NotificationClient {
            client: self.client,
            with_cross_process_lock: self.with_cross_process_lock,
            filter_by_push_rules: self.filter_by_push_rules,
            retry_decryption: self.retry_decryption,
        }
    }
}

pub struct NotificationItem {
    pub event: Arc<AnySyncTimelineEvent>,
    pub room_id: String,

    pub sender_display_name: Option<String>,
    pub sender_avatar_url: Option<String>,

    pub room_display_name: String,
    pub room_avatar_url: Option<String>,
    pub room_canonical_alias: Option<String>,

    pub is_noisy: bool,
    pub is_direct: bool,
    pub is_room_encrypted: Option<bool>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("unknown room for a notification")]
    UnknownRoom,

    #[error("invalid ruma event")]
    InvalidRumaEvent,

    #[error(transparent)]
    SdkError(#[from] matrix_sdk::Error),

    #[error(transparent)]
    StoreError(#[from] StoreError),

    #[error(transparent)]
    EncryptionSync(#[from] encryption_sync::Error),
}
