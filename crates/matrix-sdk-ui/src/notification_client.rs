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
use matrix_sdk::{room::Room, Client, ClientBuildError, SlidingSyncList, SlidingSyncMode};
use matrix_sdk_base::{deserialized_responses::TimelineEvent, RoomState, StoreError};
use ruma::{
    api::client::sync::sync_events::v4::{
        AccountDataConfig, RoomSubscription, SyncRequestListFilters,
    },
    assign,
    events::{
        room::member::StrippedRoomMemberEvent, AnyFullStateEventContent, AnyStateEvent,
        AnySyncTimelineEvent, FullStateEventContent, StateEventType,
    },
    push::Action,
    serde::Raw,
    uint, EventId, OwnedEventId, RoomId, UserId,
};
use thiserror::Error;

use crate::encryption_sync::{EncryptionSync, WithLocking};

/// A client specialized for handling push notifications received over the
/// network, for an app.
///
/// In particular, it takes care of running a full decryption sync, in case the
/// event in the notification was impossible to decrypt beforehand.
pub struct NotificationClient {
    /// SDK client that uses an in-memory state store.
    client: Client,

    /// SDK client that uses the same state store as the caller's context.
    parent_client: Client,

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
    pub async fn builder(client: Client) -> Result<NotificationClientBuilder, Error> {
        NotificationClientBuilder::new(client).await
    }

    /// Fetches the content of a notification using a mix of different
    /// strategies.
    ///
    /// This will first try to get the notification using a short-lived sliding
    /// sync, and if the sliding-sync can't find the event, then it'll use a
    /// /context query to find the event with associated member information.
    pub async fn get_notification(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<NotificationItem>, Error> {
        if let Some(found) = self.get_notification_with_sliding_sync(room_id, event_id).await? {
            Ok(Some(found))
        } else {
            self.get_notification_with_context(room_id, event_id).await
        }
    }

    /// Run an encryption sync loop, in case an event is still encrypted.
    ///
    /// Will return true if and only:
    /// - retry_decryption was enabled,
    /// - the event was encrypted,
    /// - we successfully ran an encryption sync.
    async fn maybe_retry_decryption(
        &self,
        room: &Room,
        raw_event: &Raw<AnySyncTimelineEvent>,
    ) -> Result<Option<TimelineEvent>, Error> {
        if !self.retry_decryption {
            return Ok(None);
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
            return Ok(None);
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
                Ok(()) => {
                    let new_event = room.decrypt_event(raw_event.cast_ref()).await?;
                    Ok(Some(new_event))
                }
                Err(err) => {
                    tracing::warn!(
                        "error when running encryption_sync in get_notification: {err:#}"
                    );
                    Ok(None)
                }
            },
            Err(err) => {
                tracing::warn!("error when building encryption_sync in get_notification: {err:#}",);
                Ok(None)
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
    ) -> Result<Option<RawNotificationEvent>, Error> {
        // Set up a sliding sync that only subscribes to the room that had the
        // notification, so we can figure out the full event and associated
        // information.

        let notification = Arc::new(Mutex::new(None));

        let cloned_notif = notification.clone();
        let target_event_id = event_id.to_owned();
        let timeline_event_handler =
            self.client.add_event_handler(move |raw: Raw<AnySyncTimelineEvent>| {
                match raw.get_field::<OwnedEventId>("event_id") {
                    Ok(Some(event_id)) => {
                        if event_id == target_event_id {
                            // found it! There shouldn't be a previous event before, but if there
                            // is, that should be ok to just replace it.
                            *cloned_notif.lock().unwrap() =
                                Some(RawNotificationEvent::Timeline(raw));
                        }
                    }
                    Ok(None) | Err(_) => {
                        tracing::warn!("could not get event id");
                    }
                }
                ready(())
            });

        let cloned_notif = notification.clone();
        let target_event_id = event_id.to_owned();
        let stripped_member_handler =
            self.client.add_event_handler(move |raw: Raw<StrippedRoomMemberEvent>| {
                match raw.get_field::<OwnedEventId>("event_id") {
                    Ok(Some(event_id)) => {
                        if event_id == target_event_id {
                            // found it! There shouldn't be a previous event before, but if there
                            // is, that should be ok to just replace it.
                            *cloned_notif.lock().unwrap() = Some(RawNotificationEvent::Invite(raw));
                        }
                    }
                    Ok(None) | Err(_) => {
                        tracing::warn!("could not get event id");
                    }
                }
                ready(())
            });

        let required_state = vec![
            (StateEventType::RoomAvatar, "".to_owned()),
            (StateEventType::RoomEncryption, "".to_owned()),
            (StateEventType::RoomMember, "$LAZY".to_owned()),
            (StateEventType::RoomMember, "$ME".to_owned()),
            (StateEventType::RoomCanonicalAlias, "".to_owned()),
            (StateEventType::RoomName, "".to_owned()),
            (StateEventType::RoomPowerLevels, "".to_owned()), /* necessary to build the push
                                                               * context */
        ];

        let invites = SlidingSyncList::builder("invites")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=16))
            .timeline_limit(8)
            .required_state(required_state.clone())
            .filters(Some(assign!(SyncRequestListFilters::default(), {
                is_invite: Some(true),
                is_tombstoned: Some(false),
                not_room_types: vec!["m.space".to_owned()],
            })))
            .sort(vec!["by_recency".to_owned(), "by_name".to_owned()]);

        let sync = self
            .client
            .sliding_sync(Self::CONNECTION_ID)?
            .poll_timeout(Duration::from_secs(1))
            .network_timeout(Duration::from_secs(1))
            .with_account_data_extension(
                assign!(AccountDataConfig::default(), { enabled: Some(true) }),
            )
            .add_list(invites)
            .build()
            .await?;

        sync.subscribe_to_room(
            room_id.to_owned(),
            Some(assign!(RoomSubscription::default(), {
                required_state,
                timeline_limit: Some(uint!(16))
            })),
        );

        let mut remaining_attempts = 3;

        let stream = sync.sync();
        pin_mut!(stream);

        loop {
            if stream.next().await.is_none() {
                // Sliding sync aborted early.
                break;
            }

            if notification.lock().unwrap().is_some() {
                // We got the event.
                break;
            }

            remaining_attempts -= 1;
            if remaining_attempts == 0 {
                // We're out of luck.
                break;
            }
        }

        self.client.remove_event_handler(stripped_member_handler);
        self.client.remove_event_handler(timeline_event_handler);

        let maybe_event = notification.lock().unwrap().take();
        Ok(maybe_event)
    }

    /// Get a full notification, given a room id and event id.
    ///
    /// This will run a small sliding sync to retrieve the content of the event,
    /// along with extra data to form a rich notification context.
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

        let push_actions = match &raw_event {
            RawNotificationEvent::Timeline(timeline_event) => {
                // Timeline events may be encrypted, so make sure they get decrypted first.
                if let Some(timeline_event) =
                    self.maybe_retry_decryption(&room, timeline_event).await?
                {
                    raw_event = RawNotificationEvent::Timeline(timeline_event.event.cast());
                    timeline_event.push_actions
                } else {
                    room.event_push_actions(timeline_event).await?
                }
            }
            RawNotificationEvent::Invite(invite_event) => {
                // Invite events can't be encrypted, so they should be in clear text.
                room.event_push_actions(invite_event).await?
            }
        };

        if let Some(push_actions) = &push_actions {
            if self.filter_by_push_rules && !push_actions.iter().any(|a| a.should_notify()) {
                return Ok(None);
            }
        }

        Ok(Some(
            NotificationItem::new(&room, &raw_event, push_actions.as_deref(), Vec::new()).await?,
        ))
    }

    /// Retrieve a notification using a /context query.
    ///
    /// This is for clients that are already running other sliding syncs in the
    /// same process, so that most of the contextual information for the
    /// notification should already be there. In particular, the room containing
    /// the event MUST be known (via a sliding sync for invites, or another
    /// sliding sync).
    pub async fn get_notification_with_context(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<NotificationItem>, Error> {
        tracing::info!("fetching notification event with a /context query");

        // See above comment.
        let Some(room) = self.parent_client.get_room(room_id) else {
            return Err(Error::UnknownRoom);
        };

        let (mut timeline_event, state_events) =
            room.event_with_context(event_id, true).await?.ok_or(Error::ContextMissingEvent)?;

        if let Some(decrypted_event) =
            self.maybe_retry_decryption(&room, timeline_event.event.cast_ref()).await?
        {
            timeline_event = decrypted_event;
        }

        if self.filter_by_push_rules
            && !timeline_event
                .push_actions
                .as_ref()
                .is_some_and(|actions| actions.iter().any(|a| a.should_notify()))
        {
            return Ok(None);
        }

        Ok(Some(
            NotificationItem::new(
                &room,
                &RawNotificationEvent::Timeline(timeline_event.event.cast()),
                timeline_event.push_actions.as_deref(),
                state_events,
            )
            .await?,
        ))
    }
}

/// Builder for a `NotificationClient`.
///
/// Fields have the same meaning as in `NotificationClient`.
#[derive(Clone)]
pub struct NotificationClientBuilder {
    /// SDK client that uses an in-memory state store, to be used with the
    /// sliding sync method.
    client: Client,
    /// SDK client that uses the same state store as the caller's context.
    parent_client: Client,
    retry_decryption: bool,
    with_cross_process_lock: bool,
    filter_by_push_rules: bool,
}

impl NotificationClientBuilder {
    async fn new(parent_client: Client) -> Result<Self, Error> {
        let client = parent_client.notification_client().await?;

        Ok(Self {
            client,
            parent_client,
            retry_decryption: false,
            with_cross_process_lock: false,
            filter_by_push_rules: false,
        })
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
            parent_client: self.parent_client,
            with_cross_process_lock: self.with_cross_process_lock,
            filter_by_push_rules: self.filter_by_push_rules,
            retry_decryption: self.retry_decryption,
        }
    }
}

enum RawNotificationEvent {
    Timeline(Raw<AnySyncTimelineEvent>),
    Invite(Raw<StrippedRoomMemberEvent>),
}

#[derive(Debug)]
pub enum NotificationEvent {
    Timeline(AnySyncTimelineEvent),
    Invite(StrippedRoomMemberEvent),
}

impl NotificationEvent {
    pub fn sender(&self) -> &UserId {
        match self {
            NotificationEvent::Timeline(ev) => ev.sender(),
            NotificationEvent::Invite(ev) => &ev.sender,
        }
    }
}

/// A notification with its full content.
pub struct NotificationItem {
    /// Underlying Ruma event.
    pub event: NotificationEvent,

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
        room: &Room,
        raw_event: &RawNotificationEvent,
        push_actions: Option<&[Action]>,
        state_events: Vec<Raw<AnyStateEvent>>,
    ) -> Result<Self, Error> {
        let event = match raw_event {
            RawNotificationEvent::Timeline(raw_event) => NotificationEvent::Timeline(
                raw_event.deserialize().map_err(|_| Error::InvalidRumaEvent)?,
            ),
            RawNotificationEvent::Invite(raw_event) => NotificationEvent::Invite(
                raw_event.deserialize().map_err(|_| Error::InvalidRumaEvent)?,
            ),
        };

        let sender = match room.state() {
            RoomState::Invited => room.invite_details().await?.inviter,
            _ => room.get_member_no_sync(event.sender()).await?,
        };

        let (mut sender_display_name, mut sender_avatar_url) = match &sender {
            Some(sender) => (
                sender.display_name().map(|s| s.to_owned()),
                sender.avatar_url().map(|s| s.to_string()),
            ),
            None => (None, None),
        };

        // XXX(bnjbvr): (remove before merging) Very manual processing here, but it's
        // unclear if we should hold it in the state store.
        if sender_display_name.is_none() || sender_avatar_url.is_none() {
            let sender_id = event.sender();
            for ev in state_events {
                let Ok(ev) = ev.deserialize() else {
                    continue;
                };
                if ev.sender() != sender_id {
                    continue;
                }
                if let AnyFullStateEventContent::RoomMember(FullStateEventContent::Original {
                    content,
                    ..
                }) = ev.content()
                {
                    if sender_display_name.is_none() {
                        sender_display_name = content.displayname;
                    }
                    if sender_avatar_url.is_none() {
                        sender_avatar_url = content.avatar_url.map(|url| url.to_string());
                    }
                }
            }
        }

        let is_noisy = push_actions.map(|actions| actions.iter().any(|a| a.sound().is_some()));

        let item = NotificationItem {
            event,
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
    #[error(transparent)]
    BuildingLocalClient(ClientBuildError),

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
