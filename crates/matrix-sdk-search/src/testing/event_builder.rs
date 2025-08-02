#![forbid(missing_docs)]

#[cfg(doc)]
use ruma::events::message::MessageEventContent;
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UInt,
    UserId,
    events::{
        AnyMessageLikeEvent, MessageLikeEvent, MessageLikeEventContent, MessageLikeUnsigned,
        OriginalMessageLikeEvent, RedactContent, RedactedMessageLikeEventContent,
        room::message::{MessageType, RoomMessageEventContent, TextMessageEventContent},
    },
    owned_event_id, owned_room_id, owned_user_id,
};

/// Builder for [`AnyMessageLikeEvent`] in the old m.room.message format.
/// Intended for use in testing.
/// Example:
/// ```
/// use matrix_sdk_search::testing::event_builder::OldEventBuilder;
/// use ruma::{
///     event_id,
///     events::room::message{MessageType, TextMessageEventContent},
/// };
///
/// fn test_make_old_event() {
///     let _event: AnyMessageLikeEvent = OldEventBuilder::new()
///         .event_id(event_id!("$event_id"))
///         .content(MessageType::Text(TextMessageEventContent::plain(
///             "test message",
///         )))
///         .build();
/// }
/// ```
pub struct OldEventBuilder {
    room_id: OwnedRoomId,
    unsigned: MessageLikeUnsigned<RoomMessageEventContent>,
    event_id: OwnedEventId,
    sender: OwnedUserId,
    origin_server_ts: MilliSecondsSinceUnixEpoch,
    content: Option<RoomMessageEventContent>,
}

impl Default for OldEventBuilder {
    fn default() -> Self {
        Self {
            room_id: owned_room_id!("!default_room_id:localhost"),
            unsigned: MessageLikeUnsigned::new(),
            event_id: owned_event_id!("$default_event_id:localhost"),
            sender: owned_user_id!("@default_user_id:localhost"),
            origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new_saturating(12345678)),
            content: Some(RoomMessageEventContent::new(MessageType::Text(
                TextMessageEventContent::plain("default message"),
            ))),
        }
    }
}

impl OldEventBuilder {
    /// Return a new [`OldEventBuilder`] with empty contents.
    /// Contents must be added via [`OldEventBuilder::content`] before
    /// the build method is called.
    pub fn new() -> Self {
        Self {
            room_id: owned_room_id!("!default_room_id:localhost"),
            unsigned: MessageLikeUnsigned::new(),
            event_id: owned_event_id!("$default_event_id:localhost"),
            sender: owned_user_id!("@default_user_id:localhost"),
            origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new_saturating(12345678)),
            content: None,
        }
    }

    /// Set the event's [`RoomId`]
    pub fn room_id(&mut self, id: &RoomId) -> &mut Self {
        self.room_id = id.to_owned();
        self
    }

    /// Set the event's [`EventId`]
    pub fn event_id(&mut self, id: &EventId) -> &mut Self {
        self.event_id = id.to_owned();
        self
    }

    /// Set the event's [`UserId`]
    pub fn sender(&mut self, id: &UserId) -> &mut Self {
        self.sender = id.to_owned();
        self
    }

    /// Set the event's timestamp
    pub fn timestamp(&mut self, timestamp: i32) -> &mut Self {
        self.origin_server_ts = MilliSecondsSinceUnixEpoch(UInt::new_wrapping(
            timestamp.try_into().expect("timestamp out of bounds"),
        ));
        self
    }

    /// Set the event's [`MessageLikeUnsigned`] field
    pub fn unsigned(&mut self, data: MessageLikeUnsigned<RoomMessageEventContent>) -> &mut Self {
        self.unsigned = data;
        self
    }

    /// Set the content of the event.
    pub fn content(&mut self, content: MessageType) -> &mut Self {
        self.content = Some(RoomMessageEventContent::new(content));
        self
    }

    /// Build the [`AnyMessageLikeEvent`]. Panics if no content is provided
    pub fn build(&mut self) -> AnyMessageLikeEvent {
        AnyMessageLikeEvent::RoomMessage(MessageLikeEvent::Original(OriginalMessageLikeEvent {
            content: self.content.clone().expect("No content provided."),
            event_id: self.event_id.clone(),
            origin_server_ts: self.origin_server_ts,
            room_id: self.room_id.clone(),
            sender: self.sender.clone(),
            unsigned: self.unsigned.clone(),
        }))
    }
}

/// Builder for [`AnyMessageLikeEvent`] in the new extensible message events
/// outlined in MSC1767. Intended for use in testing.
/// Example:
/// ```
/// use matrix_sdk_search::testing::event_builder::NewEventBuilder;
/// use ruma::{
///     event_id,
///     events::{AnyMessageLikeEvent, message::MessageEventContent},
/// };
///
/// fn test_make_new_event() {
///     let _event: AnyMessageLikeEvent = NewEventBuilder::new()
///         .event_id(event_id!("$event_id"))
///         .content(MessageEventContent::plain("test message"))
///         .build_as(AnyMessageLikeEvent::Message);
/// }
/// ```
pub struct NewEventBuilder<C>
where
    C: MessageLikeEventContent + RedactContent + Clone,
    <C as RedactContent>::Redacted: RedactedMessageLikeEventContent,
{
    room_id: OwnedRoomId,
    unsigned: MessageLikeUnsigned<C>,
    event_id: OwnedEventId,
    sender: OwnedUserId,
    origin_server_ts: MilliSecondsSinceUnixEpoch,
    content: Option<C>,
}

impl<C> Default for NewEventBuilder<C>
where
    C: MessageLikeEventContent + RedactContent + Clone,
    <C as RedactContent>::Redacted: RedactedMessageLikeEventContent,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<C> NewEventBuilder<C>
where
    C: MessageLikeEventContent + RedactContent + Clone,
    <C as RedactContent>::Redacted: RedactedMessageLikeEventContent,
{
    /// Return a new [`NewEventBuilder`] with empty contents.
    /// Contents must be added via [`NewEventBuilder::content`] before
    /// the build method is called.
    pub fn new() -> Self {
        Self {
            room_id: owned_room_id!("!default_room_id:localhost"),
            unsigned: MessageLikeUnsigned::new(),
            event_id: owned_event_id!("$default_event_id:localhost"),
            sender: owned_user_id!("@default_user_id:localhost"),
            origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new_saturating(12345678)),
            content: None,
        }
    }

    /// Set the event's [`RoomId`]
    pub fn room_id(&mut self, id: &RoomId) -> &mut Self {
        self.room_id = id.to_owned();
        self
    }

    /// Set the event's [`EventId`]
    pub fn event_id(&mut self, id: &EventId) -> &mut Self {
        self.event_id = id.to_owned();
        self
    }

    /// Set the event's [`UserId`]
    pub fn sender(&mut self, id: &UserId) -> &mut Self {
        self.sender = id.to_owned();
        self
    }

    /// Set the event's timestamp
    pub fn timestamp(&mut self, timestamp: i32) -> &mut Self {
        self.origin_server_ts = MilliSecondsSinceUnixEpoch(UInt::new_wrapping(
            timestamp.try_into().expect("timestamp out of bounds"),
        ));
        self
    }

    /// Set the event's [`MessageLikeUnsigned`] field
    pub fn unsigned(&mut self, data: MessageLikeUnsigned<C>) -> &mut Self {
        self.unsigned = data;
        self
    }

    /// Any [`MessageLikeEventContent`] most commonly [`MessageEventContent`]
    pub fn content(&mut self, content: C) -> &mut Self {
        self.content = Some(content);
        self
    }

    /// Build the [`AnyMessageLikeEvent`] using the function provided.
    /// Panics if no content is given.
    pub fn build_as<T>(&mut self, any_message_like_event_variant: T) -> AnyMessageLikeEvent
    where
        T: Fn(MessageLikeEvent<C>) -> AnyMessageLikeEvent,
    {
        any_message_like_event_variant(MessageLikeEvent::Original(OriginalMessageLikeEvent {
            content: self.content.clone().expect("No content given"),
            event_id: self.event_id.clone(),
            sender: self.sender.clone(),
            origin_server_ts: self.origin_server_ts,
            room_id: self.room_id.clone(),
            unsigned: self.unsigned.clone(),
        }))
    }
}
