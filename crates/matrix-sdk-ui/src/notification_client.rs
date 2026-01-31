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
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::{StreamExt as _, pin_mut};
use matrix_sdk::{
    Client, ClientBuildError, SlidingSyncList, SlidingSyncMode, room::Room, sleep::sleep,
};
use matrix_sdk_base::{RoomState, StoreError, deserialized_responses::TimelineEvent};
use ruma::{
    EventId, OwnedEventId, OwnedRoomId, RoomId, UserId,
    api::client::sync::sync_events::v5 as http,
    assign,
    events::{
        AnyFullStateEventContent, AnyMessageLikeEventContent, AnyStateEvent,
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, FullStateEventContent, StateEventType,
        TimelineEventType,
        room::{
            encrypted::OriginalSyncRoomEncryptedEvent,
            join_rules::JoinRule,
            member::{MembershipState, StrippedRoomMemberEvent},
            message::{Relation, SyncRoomMessageEvent},
        },
    },
    html::RemoveReplyFallback,
    push::Action,
    serde::Raw,
    uint,
};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, info, instrument, trace, warn};

use crate::{
    DEFAULT_SANITIZER_MODE,
    encryption_sync_service::{EncryptionSyncPermit, EncryptionSyncService, WithLocking},
    sync_service::SyncService,
};

/// What kind of process setup do we have for this notification client?
#[derive(Clone)]
pub enum NotificationProcessSetup {
    /// The notification client may run on a separate process than the rest of
    /// the app.
    ///
    /// For instance, this is the case on iOS, where notifications are handled
    /// in a separate process (the Notification Service Extension, aka NSE).
    ///
    /// In that case, a cross-process lock will be used to coordinate writes
    /// into the stores handled by the SDK.
    MultipleProcesses,

    /// The notification client runs in the same process as the rest of the
    /// `Client` performing syncs.
    ///
    /// For instance, this is the case on Android, where a notification will
    /// wake up the main app process.
    ///
    /// In that case, a smart reference to the [`SyncService`] must be provided.
    SingleProcess { sync_service: Arc<SyncService> },
}

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

    /// Is the notification client running on its own process or not?
    process_setup: NotificationProcessSetup,

    /// A mutex to serialize requests to the notifications sliding sync.
    ///
    /// If several notifications come in at the same time (e.g. network was
    /// unreachable because of airplane mode or something similar), then we
    /// need to make sure that repeated calls to `get_notification` won't
    /// cause multiple requests with the same `conn_id` we're using for
    /// notifications. This mutex solves this by sequentializing the requests.
    notification_sync_mutex: AsyncMutex<()>,

    /// A mutex to serialize requests to the encryption sliding sync that's used
    /// in case we didn't have the keys to decipher an event.
    ///
    /// Same reasoning as [`Self::notification_sync_mutex`].
    encryption_sync_mutex: AsyncMutex<()>,
}

impl NotificationClient {
    const CONNECTION_ID: &'static str = "notifications";
    const LOCK_ID: &'static str = "notifications";

    /// Create a new notification client.
    pub async fn new(
        parent_client: Client,
        process_setup: NotificationProcessSetup,
    ) -> Result<Self, Error> {
        let client = parent_client.notification_client(Self::LOCK_ID.to_owned()).await?;

        Ok(NotificationClient {
            client,
            parent_client,
            notification_sync_mutex: AsyncMutex::new(()),
            encryption_sync_mutex: AsyncMutex::new(()),
            process_setup,
        })
    }

    /// Fetches a room by its ID using the in-memory state store backed client.
    /// Useful to retrieve room information after running the limited
    /// notification client sliding sync loop.
    pub fn get_room(&self, room_id: &RoomId) -> Option<Room> {
        self.client.get_room(room_id)
    }

    /// Fetches the content of a notification.
    ///
    /// This will first try to get the notification using a short-lived sliding
    /// sync, and if the sliding-sync can't find the event, then it'll use a
    /// `/context` query to find the event with associated member information.
    ///
    /// An error result means that we couldn't resolve the notification; in that
    /// case, a dummy notification may be displayed instead.
    #[instrument(skip(self))]
    pub async fn get_notification(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<NotificationStatus, Error> {
        let status = self.get_notification_with_sliding_sync(room_id, event_id).await?;
        match status {
            NotificationStatus::Event(..) | NotificationStatus::EventFilteredOut => Ok(status),
            NotificationStatus::EventNotFound => {
                self.get_notification_with_context(room_id, event_id).await
            }
        }
    }

    /// Fetches the content of several notifications.
    ///
    /// This will first try to get the notifications using a short-lived sliding
    /// sync, and if the sliding-sync can't find the events, then it'll use a
    /// `/context` query to find the events with associated member information.
    ///
    /// An error result at the top level means that something failed when trying
    /// to set up the notification fetching.
    ///
    /// For each notification item you can also receive an error, which means
    /// something failed when trying to fetch that particular notification
    /// (decryption, fetching push actions, etc.); in that case, a dummy
    /// notification may be displayed instead.
    pub async fn get_notifications(
        &self,
        requests: &[NotificationItemsRequest],
    ) -> Result<BatchNotificationFetchingResult, Error> {
        let mut notifications = self.get_notifications_with_sliding_sync(requests).await?;

        for request in requests {
            for event_id in &request.event_ids {
                match notifications.get_mut(event_id) {
                    // If the notification for a given event wasn't found with sliding sync, try
                    // with a /context for each event.
                    Some(Ok(NotificationStatus::EventNotFound)) | None => {
                        notifications.insert(
                            event_id.to_owned(),
                            self.get_notification_with_context(&request.room_id, event_id).await,
                        );
                    }

                    _ => {}
                }
            }
        }

        Ok(notifications)
    }

    /// Run an encryption sync loop, in case an event is still encrypted.
    ///
    /// Will return `Ok(Some)` if and only if:
    /// - the event was encrypted,
    /// - we successfully ran an encryption sync or waited long enough for an
    ///   existing encryption sync to decrypt the event.
    ///
    /// Otherwise, if the event was not encrypted, or couldn't be decrypted
    /// (without causing a fatal error), will return `Ok(None)`.
    #[instrument(skip_all)]
    async fn retry_decryption(
        &self,
        room: &Room,
        raw_event: &Raw<AnySyncTimelineEvent>,
    ) -> Result<Option<TimelineEvent>, Error> {
        let event: AnySyncTimelineEvent =
            raw_event.deserialize().map_err(|_| Error::InvalidRumaEvent)?;

        if !is_event_encrypted(event.event_type()) {
            return Ok(None);
        }

        // Serialize calls to this function.
        let _guard = self.encryption_sync_mutex.lock().await;

        // The message is still encrypted, and the client is configured to retry
        // decryption.
        //
        // Spawn an `EncryptionSync` that runs two iterations of the sliding sync loop:
        // - the first iteration allows to get SS events as well as send e2ee requests.
        // - the second one let the SS homeserver forward events triggered by the
        //   sending of e2ee requests.
        //
        // Keep timeouts small for both, since we might be short on time.

        let with_locking = WithLocking::from(matches!(
            self.process_setup,
            NotificationProcessSetup::MultipleProcesses
        ));

        let push_ctx = room.push_context().await?;
        let sync_permit_guard = match &self.process_setup {
            NotificationProcessSetup::MultipleProcesses => {
                // We're running on our own process, dedicated for notifications. In that case,
                // create a dummy sync permit; we're guaranteed there's at most one since we've
                // acquired the `encryption_sync_mutex' lock here.
                let sync_permit = Arc::new(AsyncMutex::new(EncryptionSyncPermit::new()));
                sync_permit.lock_owned().await
            }

            NotificationProcessSetup::SingleProcess { sync_service } => {
                if let Some(permit_guard) = sync_service.try_get_encryption_sync_permit() {
                    permit_guard
                } else {
                    // There's already a sync service active, thus the encryption sync is already
                    // running elsewhere. As a matter of fact, if the event was encrypted, that
                    // means we were racing against the encryption sync. Wait a bit, attempt to
                    // decrypt, and carry on.

                    // We repeat the sleep 3 times at most, each iteration we
                    // double the amount of time waited, so overall we may wait up to 7 times this
                    // amount.
                    let mut wait = 200;

                    debug!("Encryption sync running in background");
                    for _ in 0..3 {
                        trace!("waiting for decryption…");

                        sleep(Duration::from_millis(wait)).await;

                        // Note: We specify the cast type in case the
                        // `experimental-encrypted-state-events` feature is enabled, which provides
                        // multiple cast implementations.
                        let new_event = room
                            .decrypt_event(
                                raw_event.cast_ref_unchecked::<OriginalSyncRoomEncryptedEvent>(),
                                push_ctx.as_ref(),
                            )
                            .await?;

                        match new_event.kind {
                            matrix_sdk::deserialized_responses::TimelineEventKind::UnableToDecrypt {
                                utd_info, ..} => {
                                if utd_info.reason.is_missing_room_key() {
                                    // Decryption error that could be caused by a missing room
                                    // key; retry in a few.
                                    wait *= 2;
                                } else {
                                    debug!("Event could not be decrypted, but waiting longer is unlikely to help: {:?}", utd_info.reason);
                                    return Ok(None);
                                }
                            }
                            _ => {
                                trace!("Waiting succeeded and event could be decrypted!");
                                return Ok(Some(new_event));
                            }
                        }
                    }

                    // We couldn't decrypt the event after waiting a few times, abort.
                    debug!("Timeout waiting for the encryption sync to decrypt notification.");
                    return Ok(None);
                }
            }
        };

        let encryption_sync = EncryptionSyncService::new(
            self.client.clone(),
            Some((Duration::from_secs(3), Duration::from_secs(4))),
            with_locking,
        )
        .await;

        // Just log out errors, but don't have them abort the notification processing:
        // an undecrypted notification is still better than no
        // notifications.

        match encryption_sync {
            Ok(sync) => match sync.run_fixed_iterations(2, sync_permit_guard).await {
                // Note: We specify the cast type in case the
                // `experimental-encrypted-state-events` feature is enabled, which provides
                // multiple cast implementations.
                Ok(()) => match room.decrypt_event(raw_event.cast_ref_unchecked::<OriginalSyncRoomEncryptedEvent>(), push_ctx.as_ref()).await {
                    Ok(new_event) => match new_event.kind {
                        matrix_sdk::deserialized_responses::TimelineEventKind::UnableToDecrypt {
                            utd_info, ..
                        } => {
                            trace!(
                                "Encryption sync failed to decrypt the event: {:?}",
                                utd_info.reason
                            );
                            Ok(None)
                        }
                        _ => {
                            trace!("Encryption sync managed to decrypt the event.");
                            Ok(Some(new_event))
                        }
                    },
                    Err(err) => {
                        trace!("Encryption sync failed to decrypt the event: {err}");
                        Ok(None)
                    }
                },
                Err(err) => {
                    warn!("Encryption sync error: {err:#}");
                    Ok(None)
                }
            },
            Err(err) => {
                warn!("Encryption sync build error: {err:#}",);
                Ok(None)
            }
        }
    }

    /// Try to run a sliding sync (without encryption) to retrieve the events
    /// from the notification.
    ///
    /// An event can either be:
    /// - an invite event,
    /// - or a non-invite event.
    ///
    /// In case it's a non-invite event, it's rather easy: we'll request
    /// explicit state that'll be useful for building the
    /// `NotificationItem`, and subscribe to the room which the notification
    /// relates to.
    ///
    /// In case it's an invite-event, it's trickier because the stripped event
    /// may not contain the event id, so we can't just match on it. Rather,
    /// we look at stripped room member events that may be fitting (i.e.
    /// match the current user and are invites), and if the SDK concludes the
    /// room was in the invited state, and we didn't find the event by id,
    /// *then* we'll use that stripped room member event.
    #[instrument(skip_all)]
    async fn try_sliding_sync(
        &self,
        requests: &[NotificationItemsRequest],
    ) -> Result<BTreeMap<OwnedEventId, (OwnedRoomId, Option<RawNotificationEvent>)>, Error> {
        // Serialize all the calls to this method by taking a lock at the beginning,
        // that will be dropped later.
        let _guard = self.notification_sync_mutex.lock().await;

        // Set up a sliding sync that only subscribes to the room that had the
        // notification, so we can figure out the full event and associated
        // information.

        let raw_notifications = Arc::new(Mutex::new(BTreeMap::new()));

        let handler_raw_notification = raw_notifications.clone();

        let requests = Arc::new(requests.iter().map(|req| (*req).clone()).collect::<Vec<_>>());

        let timeline_event_handler = self.client.add_event_handler({
            let requests = requests.clone();
            move |raw: Raw<AnySyncTimelineEvent>| async move {
                match &raw.get_field::<OwnedEventId>("event_id") {
                    Ok(Some(event_id)) => {
                        let Some(request) =
                            &requests.iter().find(|request| request.event_ids.contains(event_id))
                        else {
                            return;
                        };

                        let room_id = request.room_id.clone();

                        // found it! There shouldn't be a previous event before, but if
                        // there is, that should be ok to
                        // just replace it.
                        handler_raw_notification.lock().unwrap().insert(
                            event_id.to_owned(),
                            (room_id, Some(RawNotificationEvent::Timeline(raw))),
                        );
                    }
                    Ok(None) => {
                        warn!("a sync event had no event id");
                    }
                    Err(err) => {
                        warn!("failed to deserialize sync event id: {err}");
                    }
                }
            }
        });

        // We'll only use this event if the room is in the invited state.
        let raw_invites = Arc::new(Mutex::new(BTreeMap::new()));

        let user_id = self.client.user_id().unwrap().to_owned();
        let handler_raw_invites = raw_invites.clone();
        let handler_raw_notifications = raw_notifications.clone();
        let stripped_member_handler = self.client.add_event_handler({
            let requests = requests.clone();
            move |raw: Raw<StrippedRoomMemberEvent>| async move {
                let deserialized = match raw.deserialize() {
                    Ok(d) => d,
                    Err(err) => {
                        warn!("failed to deserialize raw stripped room member event: {err}");
                        return;
                    }
                };

                trace!("received a stripped room member event");

                // Try to match the event by event_id, as it's the most precise. In theory, we
                // shouldn't receive it, so that's a first attempt.
                match &raw.get_field::<OwnedEventId>("event_id") {
                    Ok(Some(event_id)) => {
                        let request =
                            &requests.iter().find(|request| request.event_ids.contains(event_id));
                        if request.is_none() {
                            return;
                        }
                        let room_id = request.unwrap().room_id.clone();

                        // found it! There shouldn't be a previous event before, but if
                        // there is, that should be ok to
                        // just replace it.
                        handler_raw_notifications.lock().unwrap().insert(
                            event_id.to_owned(),
                            (room_id, Some(RawNotificationEvent::Invite(raw))),
                        );
                        return;
                    }
                    Ok(None) => {
                        warn!("a room member event had no id");
                    }
                    Err(err) => {
                        warn!("failed to deserialize room member event id: {err}");
                    }
                }

                // Try to match the event by membership and state_key for the current user.
                if deserialized.content.membership == MembershipState::Invite
                    && deserialized.state_key == user_id
                {
                    trace!("found an invite event for the current user");
                    // This could be it! There might be several of these following each other, so
                    // assume it's the latest one (in sync ordering), and override a previous one if
                    // present.
                    handler_raw_invites
                        .lock()
                        .unwrap()
                        .insert(deserialized.state_key, Some(RawNotificationEvent::Invite(raw)));
                } else {
                    trace!("not an invite event, or not for the current user");
                }
            }
        });

        // Room power levels are necessary to build the push context.
        let required_state = vec![
            (StateEventType::RoomEncryption, "".to_owned()),
            (StateEventType::RoomMember, "$LAZY".to_owned()),
            (StateEventType::RoomMember, "$ME".to_owned()),
            (StateEventType::RoomCanonicalAlias, "".to_owned()),
            (StateEventType::RoomName, "".to_owned()),
            (StateEventType::RoomAvatar, "".to_owned()),
            (StateEventType::RoomPowerLevels, "".to_owned()),
            (StateEventType::RoomJoinRules, "".to_owned()),
            (StateEventType::CallMember, "*".to_owned()),
            (StateEventType::RoomCreate, "".to_owned()),
        ];

        let invites = SlidingSyncList::builder("invites")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=16))
            .timeline_limit(8)
            .required_state(required_state.clone())
            .filters(Some(assign!(http::request::ListFilters::default(), {
                is_invite: Some(true),
            })));

        let sync = self
            .client
            .sliding_sync(Self::CONNECTION_ID)?
            .poll_timeout(Duration::from_secs(1))
            .network_timeout(Duration::from_secs(3))
            .with_account_data_extension(
                assign!(http::request::AccountData::default(), { enabled: Some(true) }),
            )
            .add_list(invites)
            .build()
            .await?;

        let room_ids = requests.iter().map(|req| req.room_id.as_ref()).collect::<Vec<_>>();
        sync.subscribe_to_rooms(
            &room_ids,
            Some(assign!(http::request::RoomSubscription::default(), {
                required_state,
                timeline_limit: uint!(16)
            })),
            true,
        );

        let mut remaining_attempts = 3;

        let stream = sync.sync();
        pin_mut!(stream);

        // Sum the expected event count for each room
        let expected_event_count = requests.iter().map(|req| req.event_ids.len()).sum::<usize>();

        loop {
            if stream.next().await.is_none() {
                // Sliding sync aborted early.
                break;
            }

            if raw_notifications.lock().unwrap().len() + raw_invites.lock().unwrap().len()
                == expected_event_count
            {
                // We got the events.
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

        let mut notifications = raw_notifications.clone().lock().unwrap().clone();
        let mut missing_event_ids = Vec::new();

        // Create the list of missing event ids after the syncs.
        for request in requests.iter() {
            for event_id in &request.event_ids {
                if !notifications.contains_key(event_id) {
                    missing_event_ids.push((request.room_id.to_owned(), event_id.to_owned()));
                }
            }
        }

        // Try checking if the missing notifications could be invites.
        for (room_id, missing_event_id) in missing_event_ids {
            trace!("we didn't have a non-invite event, looking for invited room now");
            if let Some(room) = self.client.get_room(&room_id) {
                if room.state() == RoomState::Invited {
                    if let Some((_, stripped_event)) = raw_invites.lock().unwrap().pop_first() {
                        notifications.insert(
                            missing_event_id.to_owned(),
                            (room_id.to_owned(), stripped_event),
                        );
                    }
                } else {
                    debug!("the room isn't in the invited state");
                }
            } else {
                warn!(%room_id, "unknown room, can't check for invite events");
            }
        }

        let found = if notifications.len() == expected_event_count { "" } else { "not " };
        trace!("all notification events have{found} been found");

        Ok(notifications)
    }

    pub async fn get_notification_with_sliding_sync(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<NotificationStatus, Error> {
        info!("fetching notification event with a sliding sync");

        let request = NotificationItemsRequest {
            room_id: room_id.to_owned(),
            event_ids: vec![event_id.to_owned()],
        };

        let mut get_notifications_result =
            self.get_notifications_with_sliding_sync(&[request]).await?;

        get_notifications_result.remove(event_id).unwrap_or(Ok(NotificationStatus::EventNotFound))
    }

    /// Given a (decrypted or not) event, figure out whether it should be
    /// filtered out for other client-side reasons (such as the sender being
    /// ignored, for instance), and returns the corresponding
    /// [`NotificationStatus`].
    async fn compute_status(
        &self,
        room: &Room,
        push_actions: Option<&[Action]>,
        raw_event: RawNotificationEvent,
        state_events: Vec<Raw<AnyStateEvent>>,
    ) -> Result<NotificationStatus, Error> {
        if let Some(actions) = push_actions
            && !actions.iter().any(|a| a.should_notify())
        {
            // The event shouldn't notify: return early.
            return Ok(NotificationStatus::EventFilteredOut);
        }

        let notification_item =
            NotificationItem::new(room, raw_event, push_actions, state_events).await?;

        if self.client.is_user_ignored(notification_item.event.sender()).await {
            Ok(NotificationStatus::EventFilteredOut)
        } else {
            Ok(NotificationStatus::Event(Box::new(notification_item)))
        }
    }

    /// Get a list of full notifications, given a room id and event ids.
    ///
    /// This will run a small sliding sync to retrieve the content of the
    /// events, along with extra data to form a rich notification context.
    pub async fn get_notifications_with_sliding_sync(
        &self,
        requests: &[NotificationItemsRequest],
    ) -> Result<BatchNotificationFetchingResult, Error> {
        let raw_events = self.try_sliding_sync(requests).await?;

        let mut batch_result = BatchNotificationFetchingResult::new();

        for (event_id, (room_id, raw_event)) in raw_events.into_iter() {
            // At this point it should have been added by the sync, if it's not, give up.
            let Some(room) = self.client.get_room(&room_id) else { return Err(Error::UnknownRoom) };

            let Some(raw_event) = raw_event else {
                // The event was not found, so we can't build a notification.
                batch_result.insert(event_id, Ok(NotificationStatus::EventNotFound));
                continue;
            };

            let (raw_event, push_actions) = match &raw_event {
                RawNotificationEvent::Timeline(timeline_event) => {
                    // Timeline events may be encrypted, so make sure they get decrypted first.
                    match self.retry_decryption(&room, timeline_event).await {
                        Ok(Some(timeline_event)) => {
                            let push_actions = timeline_event.push_actions().map(ToOwned::to_owned);
                            (
                                RawNotificationEvent::Timeline(timeline_event.into_raw()),
                                push_actions,
                            )
                        }

                        Ok(None) => {
                            // The event was either not encrypted in the first place, or we
                            // couldn't decrypt it after retrying. Use the raw event as is.
                            match room.event_push_actions(timeline_event).await {
                                Ok(push_actions) => (raw_event.clone(), push_actions),
                                Err(err) => {
                                    // Could not get push actions.
                                    batch_result.insert(event_id, Err(err.into()));
                                    continue;
                                }
                            }
                        }

                        Err(err) => {
                            batch_result.insert(event_id, Err(err));
                            continue;
                        }
                    }
                }

                RawNotificationEvent::Invite(invite_event) => {
                    // Invite events can't be encrypted, so they should be in clear text.
                    match room.event_push_actions(invite_event).await {
                        Ok(push_actions) => {
                            (RawNotificationEvent::Invite(invite_event.clone()), push_actions)
                        }
                        Err(err) => {
                            batch_result.insert(event_id, Err(err.into()));
                            continue;
                        }
                    }
                }
            };

            let notification_status_result =
                self.compute_status(&room, push_actions.as_deref(), raw_event, Vec::new()).await;

            batch_result.insert(event_id, notification_status_result);
        }

        Ok(batch_result)
    }

    /// Retrieve a notification using a `/context` query.
    ///
    /// This is for clients that are already running other sliding syncs in the
    /// same process, so that most of the contextual information for the
    /// notification should already be there. In particular, the room containing
    /// the event MUST be known (via a sliding sync for invites, or another
    /// sliding sync).
    ///
    /// An error result means that we couldn't resolve the notification; in that
    /// case, a dummy notification may be displayed instead. A `None` result
    /// means the notification has been filtered out by the user's push
    /// rules.
    pub async fn get_notification_with_context(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<NotificationStatus, Error> {
        info!("fetching notification event with a /context query");

        // See above comment.
        let Some(room) = self.parent_client.get_room(room_id) else {
            return Err(Error::UnknownRoom);
        };

        let response = room.event_with_context(event_id, true, uint!(0), None).await?;

        let mut timeline_event = response.event.ok_or(Error::ContextMissingEvent)?;
        let state_events = response.state;

        if let Some(decrypted_event) = self.retry_decryption(&room, timeline_event.raw()).await? {
            timeline_event = decrypted_event;
        }

        let push_actions = timeline_event.push_actions().map(ToOwned::to_owned);

        self.compute_status(
            &room,
            push_actions.as_deref(),
            RawNotificationEvent::Timeline(timeline_event.into_raw()),
            state_events,
        )
        .await
    }
}

fn is_event_encrypted(event_type: TimelineEventType) -> bool {
    let is_still_encrypted = matches!(event_type, TimelineEventType::RoomEncrypted);

    #[cfg(feature = "unstable-msc3956")]
    let is_still_encrypted =
        is_still_encrypted || matches!(event_type, ruma::events::TimelineEventType::Encrypted);

    is_still_encrypted
}

#[derive(Debug)]
pub enum NotificationStatus {
    /// The event has been found and was not filtered out.
    Event(Box<NotificationItem>),
    /// The event couldn't be found in the network queries used to find it.
    EventNotFound,
    /// The event has been filtered out, either because of the user's push
    /// rules, or because the user which triggered it is ignored by the
    /// current user.
    EventFilteredOut,
}

#[derive(Debug, Clone)]
pub struct NotificationItemsRequest {
    pub room_id: OwnedRoomId,
    pub event_ids: Vec<OwnedEventId>,
}

type BatchNotificationFetchingResult = BTreeMap<OwnedEventId, Result<NotificationStatus, Error>>;

/// The Notification event as it was fetched from remote for the
/// given `event_id`, represented as Raw but decrypted, thus only
/// whether it is an invite or regular Timeline event has been
/// determined.
#[derive(Debug, Clone)]
pub enum RawNotificationEvent {
    /// The raw event for a timeline event
    Timeline(Raw<AnySyncTimelineEvent>),
    /// The notification contains an invitation with the given
    /// StrippedRoomMemberEvent (in raw here)
    Invite(Raw<StrippedRoomMemberEvent>),
}

/// The deserialized Event as it was fetched from remote for the
/// given `event_id` and after decryption (if possible).
#[derive(Debug)]
pub enum NotificationEvent {
    /// The Notification was for a TimelineEvent
    Timeline(Box<AnySyncTimelineEvent>),
    /// The Notification is an invite with the given stripped room event data
    Invite(Box<StrippedRoomMemberEvent>),
}

impl NotificationEvent {
    pub fn sender(&self) -> &UserId {
        match self {
            NotificationEvent::Timeline(ev) => ev.sender(),
            NotificationEvent::Invite(ev) => &ev.sender,
        }
    }

    /// Returns the root event id of the thread the notification event is in, if
    /// any.
    fn thread_id(&self) -> Option<OwnedEventId> {
        let NotificationEvent::Timeline(sync_timeline_event) = &self else {
            return None;
        };
        let AnySyncTimelineEvent::MessageLike(event) = sync_timeline_event.as_ref() else {
            return None;
        };
        let content = event.original_content()?;
        match content {
            AnyMessageLikeEventContent::RoomMessage(content) => match content.relates_to? {
                Relation::Thread(thread) => Some(thread.event_id),
                _ => None,
            },
            _ => None,
        }
    }
}

/// A notification with its full content.
#[derive(Debug)]
pub struct NotificationItem {
    /// Underlying Ruma event.
    pub event: NotificationEvent,

    /// The raw of the underlying event.
    pub raw_event: RawNotificationEvent,

    /// Display name of the sender.
    pub sender_display_name: Option<String>,
    /// Avatar URL of the sender.
    pub sender_avatar_url: Option<String>,
    /// Is the sender's name ambiguous?
    pub is_sender_name_ambiguous: bool,

    /// Room computed display name.
    pub room_computed_display_name: String,
    /// Room avatar URL.
    pub room_avatar_url: Option<String>,
    /// Room canonical alias.
    pub room_canonical_alias: Option<String>,
    /// Room topic.
    pub room_topic: Option<String>,
    /// Room join rule.
    ///
    /// Set to `None` if the join rule for this room is not available.
    pub room_join_rule: Option<JoinRule>,
    /// Is this room encrypted?
    pub is_room_encrypted: Option<bool>,
    /// Is this room considered a direct message?
    pub is_direct_message_room: bool,
    /// Numbers of members who joined the room.
    pub joined_members_count: u64,
    /// Is the room a space?
    pub is_space: bool,

    /// Is it a noisy notification? (i.e. does any push action contain a sound
    /// action)
    ///
    /// It is set if and only if the push actions could be determined.
    pub is_noisy: Option<bool>,
    pub has_mention: Option<bool>,
    pub thread_id: Option<OwnedEventId>,

    /// The push actions for this notification (notify, sound, highlight, etc.).
    pub actions: Option<Vec<Action>>,
}

impl NotificationItem {
    async fn new(
        room: &Room,
        raw_event: RawNotificationEvent,
        push_actions: Option<&[Action]>,
        state_events: Vec<Raw<AnyStateEvent>>,
    ) -> Result<Self, Error> {
        let event = match &raw_event {
            RawNotificationEvent::Timeline(raw_event) => {
                let mut event = raw_event.deserialize().map_err(|_| Error::InvalidRumaEvent)?;
                if let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                    SyncRoomMessageEvent::Original(ev),
                )) = &mut event
                {
                    ev.content.sanitize(DEFAULT_SANITIZER_MODE, RemoveReplyFallback::Yes);
                }
                NotificationEvent::Timeline(Box::new(event))
            }
            RawNotificationEvent::Invite(raw_event) => NotificationEvent::Invite(Box::new(
                raw_event.deserialize().map_err(|_| Error::InvalidRumaEvent)?,
            )),
        };

        let sender = match room.state() {
            RoomState::Invited => room.invite_details().await?.inviter,
            _ => room.get_member_no_sync(event.sender()).await?,
        };

        let (mut sender_display_name, mut sender_avatar_url, is_sender_name_ambiguous) =
            match &sender {
                Some(sender) => (
                    sender.display_name().map(|s| s.to_owned()),
                    sender.avatar_url().map(|s| s.to_string()),
                    sender.name_ambiguous(),
                ),
                None => (None, None, false),
            };

        if sender_display_name.is_none() || sender_avatar_url.is_none() {
            let sender_id = event.sender();
            for ev in state_events {
                let ev = match ev.deserialize() {
                    Ok(ev) => ev,
                    Err(err) => {
                        warn!("Failed to deserialize a state event: {err}");
                        continue;
                    }
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
        let has_mention = push_actions.map(|actions| actions.iter().any(|a| a.is_highlight()));
        let thread_id = event.thread_id().clone();

        let item = NotificationItem {
            event,
            raw_event,
            sender_display_name,
            sender_avatar_url,
            is_sender_name_ambiguous,
            room_computed_display_name: room.display_name().await?.to_string(),
            room_avatar_url: room.avatar_url().map(|s| s.to_string()),
            room_canonical_alias: room.canonical_alias().map(|c| c.to_string()),
            room_topic: room.topic(),
            room_join_rule: room.join_rule(),
            is_direct_message_room: room.is_direct().await?,
            is_room_encrypted: room
                .latest_encryption_state()
                .await
                .map(|state| state.is_encrypted())
                .ok(),
            joined_members_count: room.joined_members_count(),
            is_space: room.is_space(),
            is_noisy,
            has_mention,
            thread_id,
            actions: push_actions.map(|actions| actions.to_vec()),
        };

        Ok(item)
    }

    /// Returns whether this room is public or not, based on the join rule.
    ///
    /// Maybe return `None` if the join rule is not available.
    pub fn is_public(&self) -> Option<bool> {
        self.room_join_rule.as_ref().map(|rule| matches!(rule, JoinRule::Public))
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

    #[error("the event was missing in the `/context` query")]
    ContextMissingEvent,

    /// An error forwarded from the client.
    #[error(transparent)]
    SdkError(#[from] matrix_sdk::Error),

    /// An error forwarded from the underlying state store.
    #[error(transparent)]
    StoreError(#[from] StoreError),
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;
    use matrix_sdk::test_utils::mocks::MatrixMockServer;
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{event_id, room_id, user_id};

    use crate::notification_client::{NotificationItem, RawNotificationEvent};

    #[async_test]
    async fn test_notification_item_returns_thread_id() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a:b.c");
        let thread_root_event_id = event_id!("$root:b.c");
        let message = EventFactory::new()
            .room(room_id)
            .sender(user_id!("@sender:b.c"))
            .text_msg("Threaded")
            .in_thread(thread_root_event_id, event_id!("$prev:b.c"))
            .into_raw_sync();
        let room = server.sync_joined_room(&client, room_id).await;

        let raw_notification_event = RawNotificationEvent::Timeline(message);
        let notification_item =
            NotificationItem::new(&room, raw_notification_event, None, Vec::new())
                .await
                .expect("Could not create notification item");

        assert_let!(Some(thread_id) = notification_item.thread_id);
        assert_eq!(thread_id, thread_root_event_id);
    }
}
