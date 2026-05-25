use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Weak},
};

use js_int::{UInt, uint};
use matrix_sdk_common::executor::spawn;
use ruma::{
    DeviceId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId, TransactionId, UserId,
    api::client::to_device::send_event_to_device::v3::Request as ToDeviceRequest,
    events::{
        AnyStateEvent, AnySyncTimelineEvent, AnyToDeviceEventContent, StateEventType,
        StaticEventContent, ToDeviceEvent,
        event_stream::{
            StreamCancelCode, StreamCancelEventContent, StreamSubscribeEventContent,
            StreamUpdateContent, StreamUpdateEventContent, StreamUpdateOperation,
        },
        room::{
            history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
            member::{MembershipState, RoomMemberEventContent},
            message::RoomMessageEventContentWithoutRelation,
        },
    },
    serde::Raw,
    to_device::DeviceIdOrAllDevices,
};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, trace, warn};

use super::{EventStreamError, Result, StreamId, raw_content, send_to_device};
use crate::{Client, Room, room::edit::EditedContent};

/// Options for publishing a stream.
#[derive(Clone, Debug)]
pub struct EventStreamPublisherOptions {
    /// How long subscribers should treat the stream descriptor as usable.
    pub descriptor_expiry_ms: UInt,
}

impl Default for EventStreamPublisherOptions {
    fn default() -> Self {
        Self { descriptor_expiry_ms: uint!(300_000) }
    }
}

#[derive(Clone, Debug)]
struct PublisherSubscriber {
    user_id: OwnedUserId,
    device_id: OwnedDeviceId,
    next_seq: UInt,
    /// `None` means the next update for this subscriber must be a replace.
    delivered_generation: Option<u64>,
    delivered_offset: usize,
}

#[derive(Debug)]
struct PublisherInner {
    room: Room,
    current_body: String,
    generation: u64,
    descriptor_expiry_ms: UInt,
    descriptor_origin_server_ts: Option<MilliSecondsSinceUnixEpoch>,
    /// The original descriptor event body length, used as the initial delivered
    /// offset.
    original_descriptor_body_len: usize,
    subscribers: BTreeMap<(OwnedUserId, OwnedDeviceId), PublisherSubscriber>,
}

#[derive(Clone, Debug)]
struct PublisherHandle {
    state: Arc<Mutex<PublisherInner>>,
    update_sender: mpsc::UnboundedSender<()>,
}

impl PublisherHandle {
    fn new(
        client: Client,
        stream_id: StreamId,
        room: Room,
        descriptor_body: String,
        descriptor_expiry_ms: UInt,
    ) -> Self {
        let (update_sender, update_receiver) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(PublisherInner {
            room,
            original_descriptor_body_len: descriptor_body.len(),
            current_body: descriptor_body,
            generation: 0,
            descriptor_expiry_ms,
            descriptor_origin_server_ts: None,
            subscribers: Default::default(),
        }));

        let update_loop = PublisherUpdateLoop {
            client,
            state: Arc::downgrade(&state),
            stream_id,
            update_receiver,
        };
        spawn(async move {
            update_loop.run().await;
        });

        Self { state, update_sender }
    }

    async fn room(&self) -> Room {
        self.state.lock().await.room.clone()
    }

    async fn descriptor_expiry_ms(&self) -> UInt {
        self.state.lock().await.descriptor_expiry_ms
    }

    async fn set_descriptor_origin_server_ts(&self, ts: MilliSecondsSinceUnixEpoch) {
        self.state.lock().await.descriptor_origin_server_ts = Some(ts);
    }

    async fn register_subscription(
        &self,
        content: &StreamSubscribeEventContent,
        sender: &UserId,
    ) -> SubscriptionValidationResult<bool> {
        let mut publisher = self.state.lock().await;
        let key = (sender.to_owned(), content.subscriber_device_id.clone());
        let is_new = !publisher.subscribers.contains_key(&key);
        let original_descriptor_body_len = publisher.original_descriptor_body_len;

        publisher.subscribers.entry(key.clone()).or_insert_with(|| PublisherSubscriber {
            user_id: sender.to_owned(),
            device_id: content.subscriber_device_id.clone(),
            next_seq: uint!(1),
            delivered_generation: (!content.resync).then_some(0),
            delivered_offset: original_descriptor_body_len,
        });

        if let Some(subscriber) = publisher.subscribers.get_mut(&key)
            && content.resync
        {
            subscriber.delivered_generation = None;
        }

        Ok(content.resync
            || (is_new
                && (publisher.generation != 0
                    || publisher.current_body.len() != publisher.original_descriptor_body_len)))
    }

    async fn remove_subscriber(
        &self,
        subscriber_user_id: &UserId,
        subscriber_device_id: &DeviceId,
    ) -> bool {
        self.state
            .lock()
            .await
            .subscribers
            .remove(&(subscriber_user_id.to_owned(), subscriber_device_id.to_owned()))
            .is_some()
    }

    async fn queue_update(&self, update: PublisherBodyUpdate) -> Result<()> {
        {
            let mut publisher = self.state.lock().await;
            match update {
                PublisherBodyUpdate::Replace(body) => {
                    publisher.current_body = body;
                    publisher.generation = publisher.generation.saturating_add(1);
                }
                PublisherBodyUpdate::Append(body) => {
                    publisher.current_body.push_str(&body);
                }
            }
        }

        self.notify_update_loop()
    }

    fn notify_update_loop(&self) -> Result<()> {
        self.update_sender.send(()).map_err(|_| EventStreamError::UnknownStream)
    }
}

struct PublisherUpdateLoop {
    client: Client,
    state: Weak<Mutex<PublisherInner>>,
    stream_id: StreamId,
    update_receiver: mpsc::UnboundedReceiver<()>,
}

impl PublisherUpdateLoop {
    async fn run(mut self) {
        while self.update_receiver.recv().await.is_some() {
            while self.update_receiver.try_recv().is_ok() {}

            if let Err(error) = self.send_pending_updates().await {
                warn!("failed to send event stream update: {error}");
            }

            while self.update_receiver.try_recv().is_ok() {}
        }
    }

    async fn send_pending_updates(&self) -> Result<()> {
        let Some(state) = self.state.upgrade() else {
            return Ok(());
        };

        loop {
            let mut planned_updates = Vec::new();

            {
                let mut publisher = state.lock().await;

                // A subscription is bounded by its descriptor even if the subscriber never
                // explicitly cancels it.
                if publisher.descriptor_origin_server_ts.is_some_and(|origin_server_ts| {
                    descriptor_is_expired(origin_server_ts, publisher.descriptor_expiry_ms)
                }) {
                    publisher.subscribers.clear();
                    return Ok(());
                }

                for subscriber in publisher.subscribers.values() {
                    let Some(op) = make_update_for_subscriber(
                        subscriber,
                        publisher.generation,
                        publisher.current_body.as_str(),
                    ) else {
                        continue;
                    };

                    let update = StreamUpdateEventContent::new(
                        self.stream_id.room_id.clone(),
                        self.stream_id.event_id.clone(),
                        subscriber.next_seq,
                        op,
                    );

                    planned_updates.push(PlannedSubscriberUpdate {
                        user_id: subscriber.user_id.clone(),
                        device_id: subscriber.device_id.clone(),
                        content: raw_content(&update)?,
                        seq: subscriber.next_seq,
                        delivered_generation: publisher.generation,
                        delivered_offset: publisher.current_body.len(),
                    });
                }
            }

            if planned_updates.is_empty() {
                return Ok(());
            }

            trace!(
                room_id = %self.stream_id.room_id,
                event_id = %self.stream_id.event_id,
                num_updates = planned_updates.len(),
                "sending event stream updates"
            );

            // Create the to-device HTTP request payload and send it
            let mut to_device_messages = BTreeMap::new();
            for planned_update in &planned_updates {
                to_device_messages
                    .entry(planned_update.user_id.clone())
                    .or_insert_with(BTreeMap::new)
                    .insert(
                        DeviceIdOrAllDevices::DeviceId(planned_update.device_id.clone()),
                        planned_update.content.clone(),
                    );
            }
            let request = ToDeviceRequest::new_raw(
                <StreamUpdateEventContent as StaticEventContent>::TYPE.into(),
                TransactionId::new(),
                to_device_messages,
            );
            self.client.send(request).await?;

            trace!(
                room_id = %self.stream_id.room_id,
                event_id = %self.stream_id.event_id,
                num_updates = planned_updates.len(),
                "sent event stream updates"
            );

            // Now that we've successfully sent the to-device messages, update our internal
            // state for each subscriber
            let mut publisher = state.lock().await;
            for planned_update in planned_updates {
                let subscriber_key = (planned_update.user_id, planned_update.device_id);

                if let Some(subscriber) = publisher.subscribers.get_mut(&subscriber_key) {
                    subscriber.next_seq =
                        subscriber.next_seq.max(planned_update.seq.saturating_add(uint!(1)));
                    if Some(planned_update.delivered_generation) > subscriber.delivered_generation {
                        subscriber.delivered_generation = Some(planned_update.delivered_generation);
                        subscriber.delivered_offset = planned_update.delivered_offset;
                    } else if subscriber.delivered_generation
                        == Some(planned_update.delivered_generation)
                    {
                        subscriber.delivered_offset =
                            subscriber.delivered_offset.max(planned_update.delivered_offset);
                    }
                }
            }
        }
    }
}

struct PlannedSubscriberUpdate {
    user_id: OwnedUserId,
    device_id: OwnedDeviceId,
    content: Raw<AnyToDeviceEventContent>,
    seq: UInt,
    delivered_generation: u64,
    delivered_offset: usize,
}

#[derive(Debug)]
struct EventStreamPublishersInner {
    client: Client,
    publishers: Mutex<HashMap<StreamId, PublisherHandle>>,
}

/// Publisher-side event stream operations.
#[derive(Clone, Debug)]
pub struct EventStreamPublishers {
    inner: Arc<EventStreamPublishersInner>,
}

impl EventStreamPublishers {
    pub(super) fn new(client: Client) -> Self {
        let publishers = Self {
            inner: Arc::new(EventStreamPublishersInner {
                client: client.clone(),
                publishers: Default::default(),
            }),
        };

        let handler = publishers.clone();
        client.add_event_handler(move |event: ToDeviceEvent<StreamSubscribeEventContent>| {
            let handler = handler.clone();
            async move { handler.handle_subscribe(event).await }
        });

        let cancel_handler = publishers.clone();
        client.add_event_handler(move |event: ToDeviceEvent<StreamCancelEventContent>| {
            let cancel_handler = cancel_handler.clone();
            async move { cancel_handler.handle_cancel(event).await }
        });

        publishers
    }

    pub(crate) async fn create_publisher(
        &self,
        room: Room,
        stream_id: StreamId,
        descriptor_body: String,
        descriptor_expiry_ms: UInt,
    ) {
        let handle = PublisherHandle::new(
            self.inner.client.clone(),
            stream_id.clone(),
            room,
            descriptor_body,
            descriptor_expiry_ms,
        );
        self.inner.publishers.lock().await.insert(stream_id.clone(), handle);
        trace!(
            room_id = %stream_id.room_id,
            event_id = %stream_id.event_id,
            expiry_ms = ?descriptor_expiry_ms,
            "created event stream publisher"
        );
    }

    async fn handle_subscribe(&self, event: ToDeviceEvent<StreamSubscribeEventContent>) {
        let sender = event.sender;
        let content = event.content;
        let stream_id = StreamId::new(content.room_id.clone(), content.event_id.clone());

        trace!(
            room_id = %stream_id.room_id,
            event_id = %stream_id.event_id,
            subscriber_user_id = %sender,
            subscriber_device_id = %content.subscriber_device_id,
            resync = content.resync,
            "received event stream subscription"
        );

        let (publisher, should_notify) =
            match self.validate_and_register_subscription(&stream_id, &content, &sender).await {
                Ok(result) => result,
                Err((code, reason)) => {
                    debug!(
                        room_id = %stream_id.room_id,
                        event_id = %stream_id.event_id,
                        subscriber_user_id = %sender,
                        subscriber_device_id = %content.subscriber_device_id,
                        ?code,
                        reason,
                        "rejecting event stream subscription"
                    );
                    if let Err(error) =
                        self.reject_subscription(&sender, &content, code, Some(reason)).await
                    {
                        warn!("failed to send event stream cancel: {error}");
                    }

                    return;
                }
            };

        trace!(
            room_id = %stream_id.room_id,
            event_id = %stream_id.event_id,
            subscriber_user_id = %sender,
            subscriber_device_id = %content.subscriber_device_id,
            resync = content.resync,
            should_notify,
            "accepted event stream subscription"
        );

        if should_notify && let Err(error) = publisher.notify_update_loop() {
            warn!("failed to schedule event stream update: {error}");
        }
    }

    async fn handle_cancel(&self, event: ToDeviceEvent<StreamCancelEventContent>) {
        let sender = event.sender;
        let content = event.content;
        let stream_id = StreamId::new(content.room_id, content.event_id);

        let Ok(publisher) = self.publisher(&stream_id).await else {
            trace!(
                room_id = %stream_id.room_id,
                event_id = %stream_id.event_id,
                subscriber_user_id = %sender,
                subscriber_device_id = %content.subscriber_device_id,
                "ignored cancellation for unknown event stream publisher"
            );
            return;
        };

        let removed = publisher.remove_subscriber(&sender, &content.subscriber_device_id).await;
        trace!(
            room_id = %stream_id.room_id,
            event_id = %stream_id.event_id,
            subscriber_user_id = %sender,
            subscriber_device_id = %content.subscriber_device_id,
            removed,
            "handled event stream subscriber cancellation"
        );
    }

    async fn queue_update(&self, stream_id: &StreamId, update: PublisherBodyUpdate) -> Result<()> {
        let operation = match &update {
            PublisherBodyUpdate::Replace(_) => "replace",
            PublisherBodyUpdate::Append(_) => "append",
        };
        trace!(
            room_id = %stream_id.room_id,
            event_id = %stream_id.event_id,
            operation,
            "queued event stream publisher update"
        );
        self.publisher(stream_id).await?.queue_update(update).await
    }

    async fn publisher(&self, stream_id: &StreamId) -> Result<PublisherHandle> {
        self.inner
            .publishers
            .lock()
            .await
            .get(stream_id)
            .cloned()
            .ok_or(EventStreamError::UnknownStream)
    }

    async fn reject_subscription(
        &self,
        subscriber_user_id: &UserId,
        content: &StreamSubscribeEventContent,
        code: StreamCancelCode,
        reason: Option<String>,
    ) -> Result<()> {
        let mut cancel = StreamCancelEventContent::new(
            content.room_id.clone(),
            content.event_id.clone(),
            content.subscriber_device_id.clone(),
            code,
        );
        cancel.reason = reason;

        send_to_device(
            &self.inner.client,
            subscriber_user_id,
            &content.subscriber_device_id,
            cancel,
        )
        .await
    }

    /// Validate and register a subscription, returning whether the update loop
    /// should be notified.
    async fn validate_and_register_subscription(
        &self,
        stream_id: &StreamId,
        content: &StreamSubscribeEventContent,
        sender: &UserId,
    ) -> SubscriptionValidationResult<(PublisherHandle, bool)> {
        self.validate_subscriber_device(content, sender).await?;
        let publisher = self.publisher(stream_id).await.map_err(|_| {
            (StreamCancelCode::UnknownStream, "Unknown or expired stream".to_owned())
        })?;
        self.validate_stream_visibility(&publisher, stream_id, sender).await?;

        let should_notify = publisher.register_subscription(content, sender).await?;
        Ok((publisher, should_notify))
    }

    /// Check that updates would be sent to a device owned by the subscribing
    /// user.
    async fn validate_subscriber_device(
        &self,
        content: &StreamSubscribeEventContent,
        sender: &UserId,
    ) -> SubscriptionValidationResult<()> {
        if content.subscriber_device_id.as_str().is_empty() {
            return Err((
                StreamCancelCode::InvalidSubscription,
                "Empty subscriber device ID".to_owned(),
            ));
        }

        match self.inner.client.encryption().get_device(sender, &content.subscriber_device_id).await
        {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err((
                StreamCancelCode::InvalidSubscription,
                "Subscriber device does not belong to the subscribing user".to_owned(),
            )),
            Err(error) => {
                warn!("failed to look up event stream subscriber device: {error}");
                Err((
                    StreamCancelCode::InvalidSubscription,
                    "Subscriber device could not be validated".to_owned(),
                ))
            }
        }
    }

    /// Check that the subscriber is currently joined and could view the
    /// descriptor event under the room history rules in effect when that
    /// event was sent.
    async fn validate_stream_visibility(
        &self,
        publisher: &PublisherHandle,
        stream_id: &StreamId,
        sender: &UserId,
    ) -> SubscriptionValidationResult<()> {
        let room = publisher.room().await;

        match room.get_member(sender).await {
            Ok(Some(member)) if *member.membership() == MembershipState::Join => {}
            _ => {
                return Err((
                    StreamCancelCode::Forbidden,
                    "Subscriber is not joined to the room".to_owned(),
                ));
            }
        }

        // FIXME: Requiring a network request while accepting a subscription is
        // unfortunate, but the descriptor event's context is the most robust
        // way to determine whether this particular member was allowed to see
        // it. Current local state does not preserve all historical membership
        // and visibility transitions.
        let descriptor_context = room
            .event_with_context(&stream_id.event_id, false, uint!(0), None)
            .await
            .map_err(|error| {
                warn!("failed to load event stream descriptor context: {error}");
                (StreamCancelCode::UnknownStream, "Descriptor event is unavailable".to_owned())
            })?;
        let descriptor_event = descriptor_context.event.ok_or_else(|| {
            (StreamCancelCode::UnknownStream, "Descriptor event is unavailable".to_owned())
        })?;
        let descriptor_event: AnySyncTimelineEvent =
            descriptor_event.raw().deserialize().map_err(|error| {
                warn!("failed to deserialize event stream descriptor event: {error}");
                (StreamCancelCode::UnknownStream, "Descriptor event is invalid".to_owned())
            })?;
        let descriptor_ts = descriptor_event.origin_server_ts();
        publisher.set_descriptor_origin_server_ts(descriptor_ts).await;

        if descriptor_is_expired(descriptor_ts, publisher.descriptor_expiry_ms().await) {
            return Err((StreamCancelCode::UnknownStream, "Unknown or expired stream".to_owned()));
        }

        if !descriptor_is_visible_to_joined_member(
            history_visibility_from_context(&descriptor_context.state),
            membership_from_context(&descriptor_context.state, sender),
        ) {
            return Err((
                StreamCancelCode::Forbidden,
                "Subscriber cannot see the stream descriptor event".to_owned(),
            ));
        }

        Ok(())
    }
}

enum PublisherBodyUpdate {
    Replace(String),
    Append(String),
}

type SubscriptionValidationResult<T> = std::result::Result<T, (StreamCancelCode, String)>;

/// A handle for updating a stream published by this client.
#[derive(Clone, Debug)]
pub struct EventStreamPublisher {
    publishers: EventStreamPublishers,
    stream_id: StreamId,
}

impl EventStreamPublisher {
    pub(crate) fn new(publishers: EventStreamPublishers, stream_id: StreamId) -> Self {
        Self { publishers, stream_id }
    }

    /// The room event backing this published stream.
    pub fn stream_id(&self) -> &StreamId {
        &self.stream_id
    }

    /// Replace the current transient body for current subscribers.
    pub async fn replace(&self, body: impl Into<String>) -> Result<()> {
        self.publishers
            .queue_update(&self.stream_id, PublisherBodyUpdate::Replace(body.into()))
            .await
    }

    /// Append text to the current transient body for current subscribers.
    pub async fn append(&self, body: impl Into<String>) -> Result<()> {
        self.publishers
            .queue_update(&self.stream_id, PublisherBodyUpdate::Append(body.into()))
            .await
    }

    /// Finish the stream by editing the original message with final content.
    pub async fn finish(self, final_content: RoomMessageEventContentWithoutRelation) -> Result<()> {
        let room = self.publishers.publisher(&self.stream_id).await?.room().await;

        let edit = room
            .make_edit_event(&self.stream_id.event_id, EditedContent::RoomMessage(final_content))
            .await?;
        room.send(edit).await?;

        self.publishers.inner.publishers.lock().await.remove(&self.stream_id);
        trace!(
            room_id = %self.stream_id.room_id,
            event_id = %self.stream_id.event_id,
            "finalized event stream publisher"
        );

        Ok(())
    }
}

fn make_update_for_subscriber(
    subscriber: &PublisherSubscriber,
    current_generation: u64,
    current_body: &str,
) -> Option<StreamUpdateOperation> {
    if subscriber.delivered_generation != Some(current_generation) {
        return Some(StreamUpdateOperation::Replace(StreamUpdateContent::new(
            current_body.to_owned(),
        )));
    }

    if subscriber.delivered_offset == current_body.len() {
        None
    } else if subscriber.delivered_offset < current_body.len()
        && current_body.is_char_boundary(subscriber.delivered_offset)
    {
        Some(StreamUpdateOperation::Append(StreamUpdateContent::new(
            current_body[subscriber.delivered_offset..].to_owned(),
        )))
    } else {
        Some(StreamUpdateOperation::Replace(StreamUpdateContent::new(current_body.to_owned())))
    }
}

fn descriptor_is_expired(origin_server_ts: MilliSecondsSinceUnixEpoch, expiry_ms: UInt) -> bool {
    let expires_at = u64::from(origin_server_ts.0).saturating_add(u64::from(expiry_ms));
    u64::from(MilliSecondsSinceUnixEpoch::now().0) >= expires_at
}

fn descriptor_is_visible_to_joined_member(
    history_visibility: HistoryVisibility,
    membership_at_descriptor: Option<MembershipState>,
) -> bool {
    match history_visibility {
        HistoryVisibility::WorldReadable | HistoryVisibility::Shared => true,
        HistoryVisibility::Invited => matches!(
            membership_at_descriptor,
            Some(MembershipState::Invite | MembershipState::Join)
        ),
        HistoryVisibility::Joined => membership_at_descriptor == Some(MembershipState::Join),
        _ => false,
    }
}

fn history_visibility_from_context(state: &[Raw<AnyStateEvent>]) -> HistoryVisibility {
    state
        .iter()
        .find(|event| {
            event.get_field::<StateEventType>("type").ok().flatten()
                == Some(StateEventType::RoomHistoryVisibility)
        })
        .and_then(|event| {
            event.get_field::<RoomHistoryVisibilityEventContent>("content").ok().flatten()
        })
        .map(|content| content.history_visibility)
        .unwrap_or(HistoryVisibility::Shared)
}

fn membership_from_context(
    state: &[Raw<AnyStateEvent>],
    user_id: &UserId,
) -> Option<MembershipState> {
    state
        .iter()
        .find(|event| {
            event.get_field::<StateEventType>("type").ok().flatten()
                == Some(StateEventType::RoomMember)
                && event.get_field::<OwnedUserId>("state_key").ok().flatten().as_deref()
                    == Some(user_id)
        })
        .and_then(|event| event.get_field::<RoomMemberEventContent>("content").ok().flatten())
        .map(|content| content.membership)
}

#[cfg(test)]
mod tests {
    use js_int::uint;
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{
        MilliSecondsSinceUnixEpoch, event_id, events::room::message::RoomMessageEventContent,
        owned_device_id, owned_user_id, room_id,
    };
    use serde_json::json;

    use super::*;
    use crate::test_utils::mocks::{MatrixMockServer, RoomContextResponseTemplate};

    struct SubscribableStreamFixture {
        server: MatrixMockServer,
        publisher: EventStreamPublisher,
        stream_id: StreamId,
        publisher_user_id: OwnedUserId,
        subscriber_user_id: OwnedUserId,
        subscriber_device_id: OwnedDeviceId,
    }

    impl SubscribableStreamFixture {
        fn subscription_content(&self) -> StreamSubscribeEventContent {
            StreamSubscribeEventContent::new(
                self.stream_id.room_id.clone(),
                self.stream_id.event_id.clone(),
                self.subscriber_device_id.clone(),
            )
        }
    }

    async fn set_up_subscribable_stream() -> SubscribableStreamFixture {
        set_up_subscribable_stream_with_options(EventStreamPublisherOptions::default()).await
    }

    async fn set_up_subscribable_stream_with_options(
        options: EventStreamPublisherOptions,
    ) -> SubscribableStreamFixture {
        let server = MatrixMockServer::new().await;
        server.mock_crypto_endpoints_preset().await;
        let (client, subscriber_client) = server.set_up_alice_and_bob_for_encryption().await;
        let room_id = room_id!("!stream:example.org");
        let publisher_user_id = client.user_id().unwrap().to_owned();
        let subscriber_user_id = subscriber_client.user_id().unwrap().to_owned();
        let subscriber_device_id = subscriber_client.device_id().unwrap().to_owned();
        let room = server.sync_joined_room(&client, room_id).await;
        server.mock_room_state_encryption().expect_any_access_token().plain().mount().await;
        server
            .mock_room_send()
            .expect_any_access_token()
            .body_matches_partial_json(json!({ "body": "initial" }))
            .ok(event_id!("$descriptor"))
            .mock_once()
            .mount()
            .await;
        let publisher = room
            .send_streaming_message(RoomMessageEventContent::text_plain("initial"), options)
            .await
            .unwrap();
        let stream_id = publisher.stream_id().clone();

        SubscribableStreamFixture {
            server,
            publisher,
            stream_id,
            publisher_user_id,
            subscriber_user_id,
            subscriber_device_id,
        }
    }

    async fn assert_subscription_rejected(
        server: &MatrixMockServer,
        publishers: &EventStreamPublishers,
        subscriber_user_id: OwnedUserId,
        content: StreamSubscribeEventContent,
        code: StreamCancelCode,
        reason: &str,
    ) {
        let mut expected = StreamCancelEventContent::new(
            content.room_id.clone(),
            content.event_id.clone(),
            content.subscriber_device_id.clone(),
            code,
        );
        expected.reason = Some(reason.to_owned());
        let expected_body = json!({
            "messages": {
                (subscriber_user_id.as_str()): {
                    (content.subscriber_device_id.as_str()): expected,
                }
            }
        });

        let _send = server
            .mock_send_to_device()
            .expect_any_access_token()
            .for_type(<StreamCancelEventContent as StaticEventContent>::TYPE)
            .body_json(expected_body)
            .ok()
            .mock_once()
            .mount_as_scoped()
            .await;
        publishers.handle_subscribe(ToDeviceEvent::new(subscriber_user_id, content)).await;
    }

    #[test]
    fn make_update_for_subscriber_uses_generation_and_offset() {
        let mut subscriber = PublisherSubscriber {
            user_id: owned_user_id!("@alice:example.org"),
            device_id: owned_device_id!("ALICEDEVICE"),
            next_seq: uint!(1),
            delivered_generation: Some(0),
            delivered_offset: "hello".len(),
        };

        assert!(make_update_for_subscriber(&subscriber, 0, "hello").is_none());

        let Some(StreamUpdateOperation::Append(content)) =
            make_update_for_subscriber(&subscriber, 0, "hello world")
        else {
            panic!("expected append");
        };
        assert_eq!(content.body, " world");

        subscriber.delivered_generation = Some(1);
        subscriber.delivered_offset = "hello world".len();

        let Some(StreamUpdateOperation::Replace(content)) =
            make_update_for_subscriber(&subscriber, 2, "goodbye")
        else {
            panic!("expected replace");
        };
        assert_eq!(content.body, "goodbye");

        subscriber.delivered_generation = None;
        subscriber.delivered_offset = "hello".len();

        let Some(StreamUpdateOperation::Replace(content)) =
            make_update_for_subscriber(&subscriber, 2, "hello world")
        else {
            panic!("expected replace");
        };
        assert_eq!(content.body, "hello world");

        subscriber.delivered_generation = Some(2);
        subscriber.delivered_offset = 1;

        let Some(StreamUpdateOperation::Replace(content)) =
            make_update_for_subscriber(&subscriber, 2, "é")
        else {
            panic!("expected replace for a non-character-boundary offset");
        };
        assert_eq!(content.body, "é");
    }

    #[test]
    fn descriptor_visibility_follows_history_visibility_and_historical_membership() {
        for (visibility, membership, expected) in [
            (HistoryVisibility::WorldReadable, None, true),
            (HistoryVisibility::Shared, None, true),
            (HistoryVisibility::Invited, None, false),
            (HistoryVisibility::Invited, Some(MembershipState::Invite), true),
            (HistoryVisibility::Invited, Some(MembershipState::Join), true),
            (HistoryVisibility::Invited, Some(MembershipState::Leave), false),
            (HistoryVisibility::Joined, None, false),
            (HistoryVisibility::Joined, Some(MembershipState::Invite), false),
            (HistoryVisibility::Joined, Some(MembershipState::Join), true),
            (HistoryVisibility::Joined, Some(MembershipState::Leave), false),
        ] {
            assert_eq!(descriptor_is_visible_to_joined_member(visibility, membership), expected);
        }
    }

    #[test]
    fn descriptor_context_can_authorize_a_subscriber_invited_before_the_event() {
        let subscriber = owned_user_id!("@subscriber:example.org");
        let state: Vec<Raw<AnyStateEvent>> = vec![
            serde_json::from_value(json!({
                "content": { "history_visibility": "invited" },
                "event_id": "$history",
                "origin_server_ts": 1,
                "sender": "@admin:example.org",
                "state_key": "",
                "type": "m.room.history_visibility"
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "content": { "membership": "invite" },
                "event_id": "$invite",
                "origin_server_ts": 2,
                "sender": "@admin:example.org",
                "state_key": "@subscriber:example.org",
                "type": "m.room.member"
            }))
            .unwrap(),
        ];

        let history_visibility = history_visibility_from_context(&state);
        let membership = membership_from_context(&state, &subscriber);

        assert_eq!(history_visibility, HistoryVisibility::Invited);
        assert_eq!(membership, Some(MembershipState::Invite));
        assert!(descriptor_is_visible_to_joined_member(history_visibility, membership));
    }

    #[async_test]
    async fn test_sends_appended_and_replaced_updates_and_advances_delivery_state() {
        let fixture = set_up_subscribable_stream().await;
        let handle = fixture.publisher.publishers.publisher(&fixture.stream_id).await.unwrap();

        // Pretend someone just sent us (the publisher) a to-device event to subscribe
        assert!(
            !handle
                .register_subscription(&fixture.subscription_content(), &fixture.subscriber_user_id)
                .await
                .unwrap()
        );

        // Send a small append update
        {
            let _send = fixture
                .server
                .mock_send_to_device()
                .expect_any_access_token()
                .ok()
                .mock_once()
                .mount_as_scoped()
                .await;
            fixture.publisher.append(" live").await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if handle.state.lock().await.subscribers.values().next().unwrap().next_seq
                        == uint!(2)
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }

        {
            let state = handle.state.lock().await;
            let subscriber = state.subscribers.values().next().unwrap();
            assert_eq!(subscriber.next_seq, uint!(2));
            assert_eq!(subscriber.delivered_generation, Some(0));
            assert_eq!(subscriber.delivered_offset, "initial live".len());
        }

        // Send a replace update
        {
            let _send = fixture
                .server
                .mock_send_to_device()
                .expect_any_access_token()
                .ok()
                .mock_once()
                .mount_as_scoped()
                .await;
            fixture.publisher.replace("replaced").await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if handle.state.lock().await.subscribers.values().next().unwrap().next_seq
                        == uint!(3)
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }

        {
            let state = handle.state.lock().await;
            let subscriber = state.subscribers.values().next().unwrap();
            assert_eq!(subscriber.next_seq, uint!(3));
            assert_eq!(subscriber.delivered_generation, Some(1));
            assert_eq!(subscriber.delivered_offset, "replaced".len());
        }

        // Validate the to-device events were generated as expected
        let requests = fixture.server.received_requests().await.unwrap();
        let bodies: Vec<serde_json::Value> = requests
            .iter()
            .filter(|request| request.url.path().contains("/sendToDevice/"))
            .map(|request| request.body_json().unwrap())
            .collect();
        assert_eq!(bodies.len(), 2);
        assert_eq!(
            bodies[0]["messages"][fixture.subscriber_user_id.as_str()]
                [fixture.subscriber_device_id.as_str()],
            serde_json::to_value(StreamUpdateEventContent::new(
                fixture.stream_id.room_id.clone(),
                fixture.stream_id.event_id.clone(),
                uint!(1),
                StreamUpdateOperation::Append(StreamUpdateContent::new(" live".to_owned())),
            ))
            .unwrap()
        );
        assert_eq!(
            bodies[1]["messages"][fixture.subscriber_user_id.as_str()]
                [fixture.subscriber_device_id.as_str()],
            serde_json::to_value(StreamUpdateEventContent::new(
                fixture.stream_id.room_id.clone(),
                fixture.stream_id.event_id.clone(),
                uint!(2),
                StreamUpdateOperation::Replace(StreamUpdateContent::new("replaced".to_owned())),
            ))
            .unwrap()
        );
    }

    #[async_test]
    async fn test_accepts_valid_subscription() {
        let fixture = set_up_subscribable_stream().await;
        let f = EventFactory::new()
            .room(&fixture.stream_id.room_id)
            .sender(&fixture.publisher_user_id)
            .server_ts(u64::from(MilliSecondsSinceUnixEpoch::now().0));
        fixture
            .server
            .mock_get_members()
            .ok(vec![f.member(&fixture.subscriber_user_id).into_raw()])
            .mock_once()
            .mount()
            .await;
        fixture
            .server
            .mock_room_event_context()
            .room(fixture.stream_id.room_id.clone())
            .match_event_id()
            .ok(RoomContextResponseTemplate::new(
                f.text_msg("initial").event_id(&fixture.stream_id.event_id).into_event(),
            ))
            .mock_once()
            .mount()
            .await;

        fixture
            .publisher
            .publishers
            .handle_subscribe(ToDeviceEvent::new(
                fixture.subscriber_user_id.clone(),
                fixture.subscription_content(),
            ))
            .await;

        let handle = fixture.publisher.publishers.publisher(&fixture.stream_id).await.unwrap();
        assert!(
            handle
                .state
                .lock()
                .await
                .subscribers
                .contains_key(&(fixture.subscriber_user_id, fixture.subscriber_device_id))
        );
    }

    #[async_test]
    async fn test_rejects_subscription_with_empty_device_id() {
        let fixture = set_up_subscribable_stream().await;
        let content = StreamSubscribeEventContent::new(
            fixture.stream_id.room_id.clone(),
            fixture.stream_id.event_id.clone(),
            owned_device_id!(""),
        );

        assert_subscription_rejected(
            &fixture.server,
            &fixture.publisher.publishers,
            fixture.subscriber_user_id,
            content,
            StreamCancelCode::InvalidSubscription,
            "Empty subscriber device ID",
        )
        .await;
    }

    #[async_test]
    async fn test_rejects_subscriber_that_is_not_joined_to_the_room() {
        let fixture = set_up_subscribable_stream().await;
        fixture.server.mock_get_members().ok(Vec::new()).mock_once().mount().await;

        assert_subscription_rejected(
            &fixture.server,
            &fixture.publisher.publishers,
            fixture.subscriber_user_id.clone(),
            fixture.subscription_content(),
            StreamCancelCode::Forbidden,
            "Subscriber is not joined to the room",
        )
        .await;
    }

    #[async_test]
    async fn test_cancel_removes_subscriber_and_stops_updates() {
        let fixture = set_up_subscribable_stream().await;
        let handle = fixture.publisher.publishers.publisher(&fixture.stream_id).await.unwrap();
        handle
            .register_subscription(&fixture.subscription_content(), &fixture.subscriber_user_id)
            .await
            .unwrap();

        fixture
            .publisher
            .publishers
            .handle_cancel(ToDeviceEvent::new(
                fixture.subscriber_user_id.clone(),
                StreamCancelEventContent::new(
                    fixture.stream_id.room_id.clone(),
                    fixture.stream_id.event_id.clone(),
                    fixture.subscriber_device_id.clone(),
                    StreamCancelCode::UserCancelled,
                ),
            ))
            .await;
        assert!(handle.state.lock().await.subscribers.is_empty());

        let (_sender, update_receiver) = mpsc::unbounded_channel();
        let _no_send = fixture
            .server
            .mock_send_to_device()
            .expect_any_access_token()
            .ok()
            .never()
            .mount_as_scoped()
            .await;
        fixture.publisher.append(" after cancellation").await.unwrap();
        PublisherUpdateLoop {
            client: fixture.publisher.publishers.inner.client.clone(),
            state: Arc::downgrade(&handle.state),
            stream_id: fixture.stream_id,
            update_receiver,
        }
        .send_pending_updates()
        .await
        .unwrap();
    }

    #[async_test]
    async fn test_finish_removes_the_publisher_after_sending_final_content() {
        let fixture = set_up_subscribable_stream().await;
        let f = EventFactory::new().room(&fixture.stream_id.room_id);

        fixture
            .server
            .mock_room_event()
            .expect_any_access_token()
            .ok(f
                .text_msg("initial")
                .sender(&fixture.publisher_user_id)
                .event_id(&fixture.stream_id.event_id)
                .into_event())
            .expect(1)
            .mount()
            .await;
        fixture
            .server
            .mock_room_send()
            .expect_any_access_token()
            .ok(event_id!("$final"))
            .expect(1)
            .mount()
            .await;
        let finished_publisher = fixture.publisher.clone();

        fixture
            .publisher
            .finish(RoomMessageEventContentWithoutRelation::text_plain("done"))
            .await
            .unwrap();

        assert!(matches!(
            finished_publisher.publishers.publisher(&fixture.stream_id).await,
            Err(EventStreamError::UnknownStream)
        ));
        assert!(matches!(
            finished_publisher.append("too late").await,
            Err(EventStreamError::UnknownStream)
        ));
    }

    #[async_test]
    async fn test_expired_descriptor_stops_updates_to_existing_subscribers() {
        let fixture = set_up_subscribable_stream_with_options(EventStreamPublisherOptions {
            descriptor_expiry_ms: uint!(0),
        })
        .await;
        let handle = fixture.publisher.publishers.publisher(&fixture.stream_id).await.unwrap();
        handle.set_descriptor_origin_server_ts(MilliSecondsSinceUnixEpoch::now()).await;
        handle
            .register_subscription(&fixture.subscription_content(), &fixture.subscriber_user_id)
            .await
            .unwrap();

        let (_sender, update_receiver) = mpsc::unbounded_channel();
        PublisherUpdateLoop {
            client: fixture.publisher.publishers.inner.client.clone(),
            state: Arc::downgrade(&handle.state),
            stream_id: fixture.stream_id,
            update_receiver,
        }
        .send_pending_updates()
        .await
        .unwrap();

        assert!(handle.state.lock().await.subscribers.is_empty());
    }
}
