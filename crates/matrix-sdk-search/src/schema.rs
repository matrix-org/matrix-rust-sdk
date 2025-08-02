#![forbid(missing_docs)]

use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId,
    events::{
        AnyMessageLikeEvent, MessageLikeEvent, MessageLikeEventContent, RedactContent,
        RedactedMessageLikeEventContent, room::message::MessageType,
    },
};
use tantivy::{
    DateOptions, DateTime, DateTimePrecision, TantivyDocument, doc,
    schema::{Field, INDEXED, STORED, STRING, Schema, TEXT},
};

use crate::error::{IndexError, IndexSchemaError};

#[derive(Debug, Clone)]
pub(crate) struct RoomMessageSchema {
    inner: Schema,
    event_id_field: Field,
    body_field: Field,
    date_field: Field,
    sender_field: Field,
    default_search_fields: Vec<Field>,
}

impl RoomMessageSchema {
    pub(crate) fn new() -> Self {
        let mut schema = Schema::builder();
        let event_id_field = schema.add_text_field("event_id", STORED | STRING);
        let body_field = schema.add_text_field("body", TEXT);

        let date_options =
            DateOptions::from(INDEXED).set_fast().set_precision(DateTimePrecision::Seconds);

        let date_field = schema.add_date_field("date", date_options);
        let sender_field = schema.add_text_field("sender", STRING);

        let default_search_fields = vec![body_field];

        let schema = schema.build();

        Self {
            inner: schema,
            event_id_field,
            body_field,
            date_field,
            sender_field,
            default_search_fields,
        }
    }

    pub(crate) fn default_search_fields(&self) -> Vec<Field> {
        self.default_search_fields.clone()
    }

    pub(crate) fn primary_key(&self) -> Field {
        self.event_id_field
    }

    pub(crate) fn as_tantivy_schema(&self) -> Schema {
        self.inner.clone()
    }

    pub(crate) fn make_doc(
        &self,
        event: AnyMessageLikeEvent,
    ) -> Result<TantivyDocument, IndexError> {
        let (event_id, body, timestamp, sender) = self.parse_any_event(event)?;

        Ok(doc!(
            self.event_id_field => event_id.to_string(),
            self.body_field => body,
            self.date_field =>
                DateTime::from_timestamp_millis(
                    timestamp.get().into()),
            self.sender_field => sender.to_string(),
        ))
    }

    fn parse_event<C: MessageLikeEventContent + RedactContent, F>(
        &self,
        event: MessageLikeEvent<C>,
        get_body: F,
    ) -> Result<(OwnedEventId, String, MilliSecondsSinceUnixEpoch, OwnedUserId), IndexError>
    where
        <C as RedactContent>::Redacted: RedactedMessageLikeEventContent,
        F: FnOnce(&C) -> Result<String, IndexError>,
    {
        let unredacted = event.as_original().ok_or(IndexError::CannotIndexRedactedMessage)?;

        let body = get_body(&unredacted.content)?;

        Ok((
            unredacted.event_id.clone(),
            body,
            unredacted.origin_server_ts,
            unredacted.sender.clone(),
        ))
    }

    fn parse_any_event(
        &self,
        event: AnyMessageLikeEvent,
    ) -> Result<(OwnedEventId, String, MilliSecondsSinceUnixEpoch, OwnedUserId), IndexError> {
        match event {
            // old m.room.message behaviour
            AnyMessageLikeEvent::RoomMessage(event) => {
                self.parse_event(event, |content| match &content.msgtype {
                    MessageType::Text(content) => Ok(content.body.clone()),
                    _ => Err(IndexError::MessageTypeNotSupported),
                })
            }

            // new m.message behaviour
            AnyMessageLikeEvent::Message(event) => self.parse_event(event, |content| {
                content.text.find_plain().ok_or(IndexError::EmptyMessage).map(|v| v.to_owned())
            }),

            _ => Err(IndexError::MessageTypeNotSupported),
        }
    }
}

impl TryFrom<Schema> for RoomMessageSchema {
    type Error = IndexSchemaError;

    fn try_from(schema: Schema) -> Result<RoomMessageSchema, Self::Error> {
        let event_id_field = schema.get_field("event_id")?;
        let body_field = schema.get_field("body")?;
        let date_field = schema.get_field("date")?;
        let sender_field = schema.get_field("sender")?;

        let default_search_fields = vec![body_field];

        Ok(Self {
            inner: schema,
            event_id_field,
            body_field,
            date_field,
            sender_field,
            default_search_fields,
        })
    }
}
