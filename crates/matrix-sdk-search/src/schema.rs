// Copyright 2024 The Matrix.org Foundation C.I.C.
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
// See the License for the specific language governing permissions and
// limitations under the License.

//! The event cache is an abstraction layer, sitting between the Rust SDK and a
//! final client, that acts as a global observer of all the rooms, gathering and
//! inferring some extra useful information about each room. In particular, this
//! doesn't require subscribing to a specific room to get access to this
//! information.
//!
//! It's intended to be fast, robust and easy to maintain, having learned from
//! previous endeavours at implementing middle to high level features elsewhere
//! in the SDK, notably in the UI's Timeline object.
//!
//! See the [github issue](https://github.com/matrix-org/matrix-rust-sdk/issues/3058) for more
//! details about the historical reasons that led us to start writing this.

use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId,
    events::{
        AnyMessageLikeEvent, MessageLikeEvent, MessageLikeEventContent, RedactContent,
        RedactedMessageLikeEventContent, room::message::MessageType,
    },
};
use tantivy::{
    DateTime, TantivyDocument, doc,
    schema::{DateOptions, DateTimePrecision, Field, INDEXED, STORED, STRING, Schema, TEXT},
};

use crate::error::{IndexError, IndexSchemaError};

pub(crate) trait MatrixSearchIndexSchema {
    fn new() -> Self;
    fn default_search_fields(&self) -> Vec<Field>;
    fn primary_key(&self) -> Field;
    fn as_tantivy_schema(&self) -> Schema;
    fn make_doc(&self, event: AnyMessageLikeEvent) -> Result<TantivyDocument, IndexError>;
}

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

impl MatrixSearchIndexSchema for RoomMessageSchema {
    fn new() -> Self {
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

    fn default_search_fields(&self) -> Vec<Field> {
        self.default_search_fields.clone()
    }

    fn primary_key(&self) -> Field {
        self.event_id_field
    }

    fn as_tantivy_schema(&self) -> Schema {
        self.inner.clone()
    }

    fn make_doc(&self, event: AnyMessageLikeEvent) -> Result<TantivyDocument, IndexError> {
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
