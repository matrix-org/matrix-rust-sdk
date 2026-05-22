use std::{collections::HashMap, sync::Arc};

use js_int::UInt;
use ruma::{
    OwnedDeviceId, OwnedEventId, OwnedRoomId, OwnedUserId, assign,
    events::{
        ToDeviceEvent,
        event_stream::{
            StreamCancelCode, StreamCancelEventContent, StreamDescriptor,
            StreamSubscribeEventContent, StreamUpdateEventContent, StreamUpdateOperation,
        },
        room::message::{OriginalSyncRoomMessageEvent, Relation},
    },
};
use tokio::sync::{Mutex, broadcast};
use tracing::{debug, trace, warn};

use super::{EventStreamError, Result, StreamId, send_to_device};
use crate::{Client, Room};

/// State changes observed by a stream subscriber.
#[derive(Clone, Debug)]
pub enum EventStreamSubscriberUpdate {
    /// The subscriber received a complete transient body baseline.
    Replaced {
        /// The stream that changed.
        stream_id: StreamId,
        /// The complete current transient body.
        body: String,
    },

    /// The subscriber applied an append update to an existing baseline.
    Appended {
        /// The stream that changed.
        stream_id: StreamId,
        /// The text carried by the append update.
        appended_body: String,
        /// The complete current transient body after the append.
        body: String,
    },

    /// The publisher rejected the subscription or ended the stream for this subscriber.
    Cancelled {
        /// The stream that became terminal.
        stream_id: StreamId,
        /// The machine-readable cancellation reason from the publisher.
        code: StreamCancelCode,
        /// Optional human-readable context from the publisher.
        reason: Option<String>,
    },
}

#[derive(Clone, Debug)]
struct SubscriberState {
    publisher_user_id: OwnedUserId,
    publisher_device_id: OwnedDeviceId,
    subscriber_device_id: OwnedDeviceId,
    latest_seq: Option<UInt>,
    current_body: Option<String>,
    append_valid: bool,
    resync_pending: bool,
}

#[derive(Debug)]
struct EventStreamSubscriptionsInner {
    client: Client,
    subscriptions: Mutex<HashMap<StreamId, SubscriberState>>,
    updates_sender: broadcast::Sender<EventStreamSubscriberUpdate>,
}

/// Subscriber-side event stream operations.
#[derive(Clone, Debug)]
pub struct EventStreamSubscriptions {
    inner: Arc<EventStreamSubscriptionsInner>,
}

impl EventStreamSubscriptions {
    pub(super) fn new(client: Client) -> Self {
        let (updates_sender, _) = broadcast::channel(128);

        let subscriptions = Self {
            inner: Arc::new(EventStreamSubscriptionsInner {
                client: client.clone(),
                subscriptions: Default::default(),
                updates_sender,
            }),
        };

        // Receive update to-device events
        let update_handler = subscriptions.clone();
        client.add_event_handler(move |event: ToDeviceEvent<StreamUpdateEventContent>| {
            let update_handler = update_handler.clone();
            async move { update_handler.handle_update(event).await }
        });

        // Receive cancel to-device events
        let cancel_handler = subscriptions.clone();
        client.add_event_handler(move |event: ToDeviceEvent<StreamCancelEventContent>| {
            let cancel_handler = cancel_handler.clone();
            async move { cancel_handler.handle_cancel(event).await }
        });

        // We need to listen for the event we're subscribing to being edited
        let edit_handler = subscriptions.clone();
        client.add_event_handler(move |event: OriginalSyncRoomMessageEvent, room: Room| {
            let edit_handler = edit_handler.clone();
            async move { edit_handler.handle_room_message(event, room).await }
        });

        subscriptions
    }

    /// Subscribe to local subscriber-side stream state changes.
    pub fn subscribe_to_updates(&self) -> broadcast::Receiver<EventStreamSubscriberUpdate> {
        self.inner.updates_sender.subscribe()
    }

    /// Query the current in-memory transient body for a subscribed stream.
    ///
    /// This body is not room history and is not written to the event cache.
    pub async fn transient_body(&self, stream_id: &StreamId) -> Option<String> {
        self.inner
            .subscriptions
            .lock()
            .await
            .get(stream_id)
            .and_then(|state| state.current_body.clone())
    }

    /// Subscribe this device to a stream advertised by another user's event.
    pub async fn subscribe(
        &self,
        room_id: OwnedRoomId,
        event_id: OwnedEventId,
        publisher_user_id: OwnedUserId,
        descriptor: StreamDescriptor,
        descriptor_body: String,
    ) -> Result<EventStreamSubscription> {
        let own_device_id = self
            .inner
            .client
            .device_id()
            .ok_or(EventStreamError::AuthenticationRequired)?
            .to_owned();

        let stream_id = StreamId::new(room_id.clone(), event_id.clone());

        // Create our local state for managing the subscription. A duplicate subscription without
        // a resync request should not cause us to lose already-applied transient content.
        let state = SubscriberState {
            publisher_user_id: publisher_user_id.clone(),
            publisher_device_id: descriptor.device_id.clone(),
            subscriber_device_id: own_device_id.clone(),
            latest_seq: None,
            current_body: Some(descriptor_body),
            append_valid: true,
            resync_pending: false,
        };
        self.inner
            .subscriptions
            .lock()
            .await
            .entry(stream_id.clone())
            .and_modify(|state| {
                state.publisher_user_id = publisher_user_id.clone();
                state.publisher_device_id = descriptor.device_id.clone();
                state.subscriber_device_id = own_device_id.clone();
            })
            .or_insert(state);

        // Send the to-device event to let the publisher know we'd like updates
        let content = StreamSubscribeEventContent::new(room_id, event_id, own_device_id);
        send_to_device(&self.inner.client, &publisher_user_id, &descriptor.device_id, content)
            .await?;
        trace!(
            room_id = %stream_id.room_id,
            event_id = %stream_id.event_id,
            publisher_user_id = %publisher_user_id,
            publisher_device_id = %descriptor.device_id,
            "subscribed to event stream"
        );

        Ok(EventStreamSubscription { subscriptions: self.clone(), stream_id })
    }

    /// Ask the publisher for a full replacement after incremental updates could not be applied.
    async fn resync(&self, stream_id: &StreamId) -> Result<()> {
        let (publisher_user_id, publisher_device_id, subscriber_device_id) = {
            let subscriptions = self.inner.subscriptions.lock().await;
            let state = subscriptions.get(stream_id).ok_or(EventStreamError::UnknownStream)?;
            (
                state.publisher_user_id.clone(),
                state.publisher_device_id.clone(),
                state.subscriber_device_id.clone(),
            )
        };

        let content = assign!(
            StreamSubscribeEventContent::new(
                stream_id.room_id.clone(),
                stream_id.event_id.clone(),
                subscriber_device_id,
            ),
            { resync: true }
        );

        send_to_device(&self.inner.client, &publisher_user_id, &publisher_device_id, content)
            .await?;
        trace!(
            room_id = %stream_id.room_id,
            event_id = %stream_id.event_id,
            publisher_user_id = %publisher_user_id,
            publisher_device_id = %publisher_device_id,
            "requested event stream resync"
        );

        Ok(())
    }

    /// Stop tracking a subscription locally and notify the publisher.
    pub async fn unsubscribe(&self, stream_id: &StreamId) {
        let Some(state) = self.inner.subscriptions.lock().await.remove(stream_id) else {
            return;
        };

        let content = StreamCancelEventContent::new(
            stream_id.room_id.clone(),
            stream_id.event_id.clone(),
            state.subscriber_device_id,
            StreamCancelCode::UserCancelled,
        );

        if let Err(error) = send_to_device(
            &self.inner.client,
            &state.publisher_user_id,
            &state.publisher_device_id,
            content,
        )
        .await
        {
            warn!("failed to send event stream unsubscribe: {error}");
        } else {
            trace!(
                room_id = %stream_id.room_id,
                event_id = %stream_id.event_id,
                publisher_user_id = %state.publisher_user_id,
                publisher_device_id = %state.publisher_device_id,
                "unsubscribed from event stream"
            );
        }
    }

    async fn handle_update(&self, event: ToDeviceEvent<StreamUpdateEventContent>) {
        let sender = event.sender;
        let content = event.content;
        let stream_id = StreamId::new(content.room_id.clone(), content.event_id.clone());

        let update = {
            let mut subscriptions = self.inner.subscriptions.lock().await;
            let Some(state) = subscriptions.get_mut(&stream_id) else {
                trace!(
                    room_id = %stream_id.room_id,
                    event_id = %stream_id.event_id,
                    sender = %sender,
                    seq = ?content.seq,
                    "ignored update for untracked event stream"
                );
                return;
            };

            if state.publisher_user_id != sender {
                debug!(
                    room_id = %stream_id.room_id,
                    event_id = %stream_id.event_id,
                    sender = %sender,
                    expected_sender = %state.publisher_user_id,
                    seq = ?content.seq,
                    "ignored event stream update from unexpected sender"
                );
                return;
            }

            if state.latest_seq.is_some_and(|seq| content.seq <= seq) {
                trace!(
                    room_id = %stream_id.room_id,
                    event_id = %stream_id.event_id,
                    seq = ?content.seq,
                    latest_seq = ?state.latest_seq,
                    "ignored stale event stream update"
                );
                return;
            }

            let mut should_resync = false;
            let update = match content.op {
                StreamUpdateOperation::Replace(new_content) => {
                    trace!(
                        room_id = %stream_id.room_id,
                        event_id = %stream_id.event_id,
                        seq = ?content.seq,
                        "applying event stream replacement update"
                    );
                    state.latest_seq = Some(content.seq);
                    state.current_body = Some(new_content.body.clone());
                    state.append_valid = true;
                    state.resync_pending = false;

                    Some(EventStreamSubscriberUpdate::Replaced {
                        stream_id: stream_id.clone(),
                        body: new_content.body,
                    })
                }
                StreamUpdateOperation::Append(append) => {
                    let expected_next = state
                        .latest_seq
                        .and_then(|seq| u64::try_from(seq).ok())
                        .and_then(|seq| UInt::try_from(seq + 1).ok())
                        .unwrap_or(js_int::uint!(1));

                    let can_append = state.append_valid
                        && state.current_body.is_some()
                        && expected_next == content.seq;

                    if !can_append {
                        if !state.resync_pending {
                            debug!(
                                room_id = %stream_id.room_id,
                                event_id = %stream_id.event_id,
                                seq = ?content.seq,
                                expected_next = ?expected_next,
                                "cannot apply event stream append update; requesting resync"
                            );
                            state.resync_pending = true;
                            should_resync = true;
                        }
                        state.append_valid = false;
                        state.latest_seq = Some(content.seq);

                        None
                    } else {
                        trace!(
                            room_id = %stream_id.room_id,
                            event_id = %stream_id.event_id,
                            seq = ?content.seq,
                            "applying event stream append update"
                        );
                        let current = state.current_body.as_mut().expect("checked above");
                        current.push_str(&append.body);
                        let body = current.clone();
                        state.latest_seq = Some(content.seq);

                        Some(EventStreamSubscriberUpdate::Appended {
                            stream_id: stream_id.clone(),
                            appended_body: append.body,
                            body,
                        })
                    }
                }
                _ => {
                    if !state.resync_pending {
                        debug!(
                            room_id = %stream_id.room_id,
                            event_id = %stream_id.event_id,
                            seq = ?content.seq,
                            "unsupported event stream update operation; requesting resync"
                        );
                        state.resync_pending = true;
                        should_resync = true;
                    }
                    state.append_valid = false;
                    state.latest_seq = Some(content.seq);

                    None
                }
            };

            (update, should_resync)
        };

        if let Some(update) = update.0 {
            let _ = self.inner.updates_sender.send(update);
        }

        if update.1 {
            if let Err(error) = self.resync(&stream_id).await {
                warn!("failed to resync event stream: {error}");
                if let Some(state) = self.inner.subscriptions.lock().await.get_mut(&stream_id) {
                    state.resync_pending = false;
                }
            }
        }
    }

    async fn handle_room_message(&self, event: OriginalSyncRoomMessageEvent, room: Room) {
        let Some(Relation::Replacement(replacement)) = event.content.relates_to else {
            return;
        };

        let stream_id = StreamId::new(room.room_id().to_owned(), replacement.event_id);
        let mut subscriptions = self.inner.subscriptions.lock().await;
        // This handler sees replacement relations directly, before timeline edit
        // validation rejects edits sent by anyone other than the original sender. Make sure it's
        // a valid edit before handling it
        if let Some(state) = subscriptions.get(&stream_id) {
            if state.publisher_user_id != event.sender {
                debug!(
                    room_id = %stream_id.room_id,
                    event_id = %stream_id.event_id,
                    sender = %event.sender,
                    expected_sender = %state.publisher_user_id,
                    "ignored event stream final replacement from unexpected sender"
                );
                return;
            }

            subscriptions.remove(&stream_id);
            trace!(
                room_id = %stream_id.room_id,
                event_id = %stream_id.event_id,
                "stopped tracking finalized event stream"
            );
        }
    }

    async fn handle_cancel(&self, event: ToDeviceEvent<StreamCancelEventContent>) {
        let sender = event.sender;
        let content = event.content;
        let stream_id = StreamId::new(content.room_id.clone(), content.event_id.clone());

        {
            let mut subscriptions = self.inner.subscriptions.lock().await;
            let Some(state) = subscriptions.get(&stream_id) else {
                trace!(
                    room_id = %stream_id.room_id,
                    event_id = %stream_id.event_id,
                    sender = %sender,
                    "ignored cancellation for untracked event stream"
                );
                return;
            };

            if state.publisher_user_id != sender
                || state.subscriber_device_id != content.subscriber_device_id
            {
                debug!(
                    room_id = %stream_id.room_id,
                    event_id = %stream_id.event_id,
                    sender = %sender,
                    subscriber_device_id = %content.subscriber_device_id,
                    "ignored event stream cancellation from unexpected sender or device"
                );
                return;
            }

            subscriptions.remove(&stream_id);
        }

        trace!(
            room_id = %stream_id.room_id,
            event_id = %stream_id.event_id,
            ?content.code,
            "event stream subscription cancelled"
        );
        let _ = self.inner.updates_sender.send(EventStreamSubscriberUpdate::Cancelled {
            stream_id,
            code: content.code,
            reason: content.reason,
        });
    }
}

/// A handle for a stream subscription owned by this client.
#[derive(Clone, Debug)]
pub struct EventStreamSubscription {
    subscriptions: EventStreamSubscriptions,
    stream_id: StreamId,
}

impl EventStreamSubscription {
    /// The room event backing this subscribed stream.
    pub fn stream_id(&self) -> &StreamId {
        &self.stream_id
    }

    /// Stop tracking this stream on this client.
    pub async fn unsubscribe(self) {
        self.subscriptions.unsubscribe(&self.stream_id).await;
    }
}

#[cfg(test)]
mod tests {
    use js_int::uint;
    use matrix_sdk_base::RoomState;
    use matrix_sdk_test::async_test;
    use ruma::{
        MilliSecondsSinceUnixEpoch, event_id,
        events::{
            ToDeviceEvent,
            event_stream::{StreamUpdateContent, StreamUpdateEventContent, StreamUpdateOperation},
            room::message::OriginalSyncRoomMessageEvent,
        },
        owned_device_id, owned_user_id, room_id,
    };
    use serde_json::json;

    use super::*;
    use crate::test_utils::{logged_in_client, mocks::MatrixMockServer};

    async fn track_stream(subscriptions: &EventStreamSubscriptions, stream_id: &StreamId) {
        subscriptions.inner.subscriptions.lock().await.insert(
            stream_id.clone(),
            SubscriberState {
                publisher_user_id: owned_user_id!("@publisher:example.org"),
                publisher_device_id: owned_device_id!("PUBLISHER"),
                subscriber_device_id: owned_device_id!("SUBSCRIBER"),
                latest_seq: None,
                current_body: Some("initial".to_owned()),
                append_valid: true,
                resync_pending: false,
            },
        );
    }

    #[async_test]
    async fn test_applies_updates_until_the_stream_is_finalized() {
        let client = logged_in_client(None).await;
        let room_id = room_id!("!room:example.org");
        client.base_client().get_or_create_room(room_id, RoomState::Joined);
        let room = client.get_room(room_id).unwrap();
        let subscriptions = EventStreamSubscriptions::new(client);
        let stream_id = StreamId::new(room_id.to_owned(), event_id!("$stream").to_owned());
        track_stream(&subscriptions, &stream_id).await;

        let update = |seq, operation| {
            ToDeviceEvent::new(
                owned_user_id!("@publisher:example.org"),
                StreamUpdateEventContent::new(
                    stream_id.room_id.clone(),
                    stream_id.event_id.clone(),
                    seq,
                    operation,
                ),
            )
        };

        subscriptions
            .handle_update(update(
                uint!(1),
                StreamUpdateOperation::Append(StreamUpdateContent::new(" one".to_owned())),
            ))
            .await;
        assert_eq!(subscriptions.transient_body(&stream_id).await.as_deref(), Some("initial one"));

        subscriptions
            .handle_update(update(
                uint!(2),
                StreamUpdateOperation::Append(StreamUpdateContent::new(" two".to_owned())),
            ))
            .await;
        assert_eq!(
            subscriptions.transient_body(&stream_id).await.as_deref(),
            Some("initial one two")
        );

        subscriptions
            .handle_update(update(
                uint!(3),
                StreamUpdateOperation::Replace(StreamUpdateContent::new("replaced".to_owned())),
            ))
            .await;
        assert_eq!(subscriptions.transient_body(&stream_id).await.as_deref(), Some("replaced"));

        subscriptions
            .handle_update(update(
                uint!(4),
                StreamUpdateOperation::Append(StreamUpdateContent::new(" again".to_owned())),
            ))
            .await;
        assert_eq!(
            subscriptions.transient_body(&stream_id).await.as_deref(),
            Some("replaced again")
        );

        let final_edit: OriginalSyncRoomMessageEvent = serde_json::from_value(json!({
            "content": {
                "msgtype": "m.text", "body": "* done",
                "m.new_content": { "msgtype": "m.text", "body": "done" },
                "m.relates_to": { "rel_type": "m.replace", "event_id": "$stream" }
            },
            "event_id": "$edit",
            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
            "sender": "@publisher:example.org",
            "type": "m.room.message"
        }))
        .unwrap();

        subscriptions.handle_room_message(final_edit, room).await;

        assert!(subscriptions.transient_body(&stream_id).await.is_none());
    }

    #[async_test]
    async fn test_ignores_updates_from_a_different_user() {
        let subscriptions = EventStreamSubscriptions::new(logged_in_client(None).await);
        let stream_id = StreamId::new(
            room_id!("!room:example.org").to_owned(),
            event_id!("$stream").to_owned(),
        );
        track_stream(&subscriptions, &stream_id).await;

        let update = |body: &str| {
            StreamUpdateEventContent::new(
                stream_id.room_id.clone(),
                stream_id.event_id.clone(),
                uint!(1),
                StreamUpdateOperation::Replace(StreamUpdateContent::new(body.to_owned())),
            )
        };

        subscriptions
            .handle_update(ToDeviceEvent::new(
                owned_user_id!("@attacker:example.org"),
                update("tampered"),
            ))
            .await;
        assert_eq!(subscriptions.transient_body(&stream_id).await.as_deref(), Some("initial"));

        subscriptions
            .handle_update(ToDeviceEvent::new(
                owned_user_id!("@publisher:example.org"),
                update("accepted"),
            ))
            .await;
        assert_eq!(subscriptions.transient_body(&stream_id).await.as_deref(), Some("accepted"));
    }

    #[async_test]
    async fn test_invalid_final_replacement_from_another_user_keeps_subscription_state() {
        let client = logged_in_client(None).await;
        let room_id = room_id!("!room:example.org");
        client.base_client().get_or_create_room(room_id, RoomState::Joined);
        let room = client.get_room(room_id).unwrap();
        let subscriptions = EventStreamSubscriptions::new(client);
        let stream_id = StreamId::new(room_id.to_owned(), event_id!("$stream").to_owned());
        track_stream(&subscriptions, &stream_id).await;

        let event: OriginalSyncRoomMessageEvent = serde_json::from_value(json!({
            "content": {
                "msgtype": "m.text",
                "body": "* done",
                "m.new_content": { "msgtype": "m.text", "body": "done" },
                "m.relates_to": { "rel_type": "m.replace", "event_id": "$stream" }
            },
            "event_id": "$invalid_edit",
            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
            "sender": "@attacker:example.org",
            "type": "m.room.message"
        }))
        .unwrap();

        subscriptions.handle_room_message(event, room).await;

        assert_eq!(subscriptions.transient_body(&stream_id).await.as_deref(), Some("initial"));
    }

    #[async_test]
    async fn test_stale_and_duplicate_updates_do_not_change_the_transient_body() {
        let subscriptions = EventStreamSubscriptions::new(logged_in_client(None).await);
        let stream_id = StreamId::new(
            room_id!("!room:example.org").to_owned(),
            event_id!("$stream").to_owned(),
        );
        track_stream(&subscriptions, &stream_id).await;

        let update = |seq, body: &str| {
            ToDeviceEvent::new(
                owned_user_id!("@publisher:example.org"),
                StreamUpdateEventContent::new(
                    stream_id.room_id.clone(),
                    stream_id.event_id.clone(),
                    seq,
                    StreamUpdateOperation::Replace(StreamUpdateContent::new(body.to_owned())),
                ),
            )
        };

        subscriptions.handle_update(update(uint!(2), "latest")).await;
        subscriptions.handle_update(update(uint!(2), "duplicate")).await;
        subscriptions.handle_update(update(uint!(1), "stale")).await;

        assert_eq!(subscriptions.transient_body(&stream_id).await.as_deref(), Some("latest"));
    }

    #[async_test]
    async fn test_duplicate_subscription_preserves_the_transient_baseline() {
        let server = MatrixMockServer::new().await;
        let subscriptions = EventStreamSubscriptions::new(server.client_builder().build().await);
        let stream_id = StreamId::new(
            room_id!("!room:example.org").to_owned(),
            event_id!("$stream").to_owned(),
        );
        track_stream(&subscriptions, &stream_id).await;

        let update = |seq, appended_body: &str| {
            ToDeviceEvent::new(
                owned_user_id!("@publisher:example.org"),
                StreamUpdateEventContent::new(
                    stream_id.room_id.clone(),
                    stream_id.event_id.clone(),
                    seq,
                    StreamUpdateOperation::Append(StreamUpdateContent::new(
                        appended_body.to_owned(),
                    )),
                ),
            )
        };

        subscriptions.handle_update(update(uint!(1), " live")).await;
        assert_eq!(subscriptions.transient_body(&stream_id).await.as_deref(), Some("initial live"));

        {
            let _renewal = server.mock_send_to_device().ok().mock_once().mount_as_scoped().await;
            subscriptions
                .subscribe(
                    stream_id.room_id.clone(),
                    stream_id.event_id.clone(),
                    owned_user_id!("@publisher:example.org"),
                    StreamDescriptor::new(owned_device_id!("PUBLISHER")),
                    "initial".to_owned(),
                )
                .await
                .unwrap();
        }

        assert_eq!(subscriptions.transient_body(&stream_id).await.as_deref(), Some("initial live"));

        {
            let _no_resync = server.mock_send_to_device().ok().never().mount_as_scoped().await;
            subscriptions.handle_update(update(uint!(2), " update")).await;
        }
        assert_eq!(
            subscriptions.transient_body(&stream_id).await.as_deref(),
            Some("initial live update")
        );
    }

    #[async_test]
    async fn test_requests_one_resync_until_a_replacement_restores_the_baseline() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let subscriptions = EventStreamSubscriptions::new(client);
        let stream_id = StreamId::new(
            room_id!("!room:example.org").to_owned(),
            event_id!("$stream").to_owned(),
        );
        track_stream(&subscriptions, &stream_id).await;

        let update = |seq, operation| {
            ToDeviceEvent::new(
                owned_user_id!("@publisher:example.org"),
                StreamUpdateEventContent::new(
                    stream_id.room_id.clone(),
                    stream_id.event_id.clone(),
                    seq,
                    operation,
                ),
            )
        };

        // Missing seq 1 makes this append unusable and triggers the first resync request.
        {
            let _resync = server.mock_send_to_device().ok().mock_once().mount_as_scoped().await;
            subscriptions
                .handle_update(update(
                    uint!(2),
                    StreamUpdateOperation::Append(StreamUpdateContent::new("missing".to_owned())),
                ))
                .await;
        }

        // The baseline is still invalid, but the outstanding resync suppresses a second request.
        {
            let _no_resync = server.mock_send_to_device().ok().never().mount_as_scoped().await;
            subscriptions
                .handle_update(update(
                    uint!(3),
                    StreamUpdateOperation::Append(StreamUpdateContent::new(
                        "still missing".to_owned(),
                    )),
                ))
                .await;
        }

        // This is the resync update, we're good again
        {
            let _no_resync = server.mock_send_to_device().ok().never().mount_as_scoped().await;
            subscriptions
                .handle_update(update(
                    uint!(4),
                    StreamUpdateOperation::Replace(StreamUpdateContent::new("restored".to_owned())),
                ))
                .await;
        }

        // This new gap makes the append unusable and triggers a second resync request.
        {
            let _resync = server.mock_send_to_device().ok().mock_once().mount_as_scoped().await;
            subscriptions
                .handle_update(update(
                    uint!(6),
                    StreamUpdateOperation::Append(StreamUpdateContent::new("new gap".to_owned())),
                ))
                .await;
        }
    }

    #[async_test]
    async fn test_updates_after_final_replacement_are_ignored() {
        let client = logged_in_client(None).await;
        let room_id = room_id!("!room:example.org");
        client.base_client().get_or_create_room(room_id, RoomState::Joined);
        let room = client.get_room(room_id).unwrap();
        let subscriptions = EventStreamSubscriptions::new(client);
        let stream_id = StreamId::new(room_id.to_owned(), event_id!("$stream").to_owned());
        track_stream(&subscriptions, &stream_id).await;

        let event: OriginalSyncRoomMessageEvent = serde_json::from_value(json!({
            "content": {
                "msgtype": "m.text",
                "body": "* done",
                "m.new_content": { "msgtype": "m.text", "body": "done" },
                "m.relates_to": { "rel_type": "m.replace", "event_id": "$stream" }
            },
            "event_id": "$edit",
            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
            "sender": "@publisher:example.org",
            "type": "m.room.message"
        }))
        .unwrap();
        subscriptions.handle_room_message(event, room).await;

        subscriptions
            .handle_update(ToDeviceEvent::new(
                owned_user_id!("@publisher:example.org"),
                StreamUpdateEventContent::new(
                    stream_id.room_id.clone(),
                    stream_id.event_id.clone(),
                    uint!(1),
                    StreamUpdateOperation::Replace(StreamUpdateContent::new("late".to_owned())),
                ),
            ))
            .await;

        assert!(subscriptions.transient_body(&stream_id).await.is_none());
    }
}
