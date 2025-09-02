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

use ruma::{
    events::{
        AnySyncMessageLikeEvent, SyncMessageLikeEvent,
        room::{
            message::{MessageType, RoomMessageEventContent},
            redaction::SyncRoomRedactionEvent,
        },
    },
    room_version_rules::RedactionRules,
};
use tantivy::{
    DateTime, TantivyDocument, doc,
    schema::{DateOptions, DateTimePrecision, Field, INDEXED, STORED, STRING, Schema, TEXT},
};
use tracing::trace;

use crate::{
    error::{IndexError, IndexSchemaError},
    index::RoomIndexOperation,
};

pub(crate) trait MatrixSearchIndexSchema {
    fn new() -> Self;
    fn default_search_fields(&self) -> Vec<Field>;
    fn primary_key(&self) -> Field;
    fn as_tantivy_schema(&self) -> Schema;
    fn handle_event(
        &self,
        event: AnySyncMessageLikeEvent,
    ) -> Result<RoomIndexOperation, IndexError>;
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
    /// Given an [`SyncMessageLikeEvent<RoomMessageEventContent>`] and a
    /// function to convert the content into a String to be indexed, return
    /// a [`TantivyDocument`] to index.
    fn make_doc(
        &self,
        event: SyncMessageLikeEvent<RoomMessageEventContent>,
    ) -> Result<TantivyDocument, IndexError> {
        let unredacted = event.as_original().ok_or(IndexError::CannotIndexRedactedMessage)?;

        let body = match &unredacted.content.msgtype {
            MessageType::Text(content) => Ok(content.body.clone()),
            _ => Err(IndexError::MessageTypeNotSupported),
        }?;

        Ok(doc!(
            self.event_id_field => unredacted.event_id.to_string(),
            self.body_field => body,
            self.date_field =>
                DateTime::from_timestamp_millis(
                    unredacted.origin_server_ts.get().into()),
            self.sender_field => unredacted.sender.to_string(),
        ))
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

    fn handle_event(
        &self,
        event: AnySyncMessageLikeEvent,
        rules: &RedactionRules,
    ) -> Result<RoomIndexOperation, IndexError> {
        match event {
            // m.room.message behaviour
            AnySyncMessageLikeEvent::RoomMessage(event) => {
                self.make_doc(event).map(RoomIndexOperation::Add)
            }

            AnySyncMessageLikeEvent::RoomRedaction(redaction_event) => {
                if let SyncRoomRedactionEvent::Original(redaction_event) = redaction_event {
                    if let Some(redacted_event_id) = redaction_event.redacts(rules) {
                        Ok(RoomIndexOperation::Remove(redacted_event_id))
                    } else {
                        // If not acting on anything, we can just ignore it.
                        trace!("Room redaction in indexing redacts nothing, ignoring.");
                        Ok(RoomIndexOperation::Noop)
                    }
                } else {
                    // If redaction itself is redacted, we can ignore it.
                    Ok(RoomIndexOperation::Noop)
                }
            }

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
