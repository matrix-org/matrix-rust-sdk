use matrix_sdk_base::deserialized_responses::TimelineEvent;
use ruma::{
    EventId, assign,
    events::{
        AnyMessageLikeEventContent, AnySyncTimelineEvent, event_stream::StreamDescriptor,
        room::message::RoomMessageEventContent,
    },
};

use super::Room;
use crate::event_streams::{
    EventStreamError, EventStreamPublisher, EventStreamPublisherOptions, EventStreamSubscription,
    Result, StreamId,
};

impl Room {
    /// Send a room message and return a publisher for sending transient updates
    /// to it.
    pub async fn send_streaming_message(
        &self,
        message_content: RoomMessageEventContent,
        options: EventStreamPublisherOptions,
    ) -> Result<EventStreamPublisher> {
        let device_id =
            self.client().device_id().ok_or(EventStreamError::AuthenticationRequired)?.to_owned();
        let content_with_descriptor = assign!(message_content.clone(), {
            stream: Some(assign!(StreamDescriptor::new(device_id.clone()), {
                expiry_ms: Some(options.descriptor_expiry_ms),
            })),
        });
        let response = self.send(content_with_descriptor).await?;

        let stream_id = StreamId::new(self.room_id().to_owned(), response.response.event_id);
        let publishers = self.client().event_streams().publishers();
        publishers
            .create_publisher(
                self.clone(),
                stream_id.clone(),
                message_content.msgtype.body().to_owned(),
                options.descriptor_expiry_ms,
            )
            .await;

        Ok(EventStreamPublisher::new(publishers, stream_id))
    }

    /// Subscribe this device to a stream advertised by an event in this room.
    pub async fn subscribe_to_event_stream(
        &self,
        event_id: &EventId,
    ) -> Result<EventStreamSubscription> {
        let event = self.load_or_fetch_event(event_id, None).await?;
        let publisher_user_id = event.sender().ok_or(EventStreamError::MissingDescriptorSender)?;
        let (descriptor, descriptor_body) = stream_descriptor_and_body(&event)?;

        self.client()
            .event_streams()
            .subscriptions()
            .subscribe(
                self.room_id().to_owned(),
                event_id.to_owned(),
                publisher_user_id,
                descriptor,
                descriptor_body,
            )
            .await
    }

    /// Stop tracking the stream advertised by an event in this room.
    pub async fn unsubscribe_from_event_stream(&self, event_id: &EventId) {
        self.client()
            .event_streams()
            .subscriptions()
            .unsubscribe(&StreamId::new(self.room_id().to_owned(), event_id.to_owned()))
            .await;
    }
}

fn stream_descriptor_and_body(event: &TimelineEvent) -> Result<(StreamDescriptor, String)> {
    match event.raw().deserialize()? {
        AnySyncTimelineEvent::MessageLike(event) => {
            let Some(AnyMessageLikeEventContent::RoomMessage(content)) = event.original_content()
            else {
                return Err(EventStreamError::InvalidDescriptorEvent);
            };

            let Some(descriptor) = content.stream else {
                return Err(EventStreamError::InvalidDescriptorEvent);
            };

            Ok((descriptor, content.msgtype.body().to_owned()))
        }
        _ => Err(EventStreamError::InvalidDescriptorEvent),
    }
}
