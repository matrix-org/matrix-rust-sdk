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

use ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent, Relation};
use tantivy::{
    DateTime, TantivyDocument, doc,
    schema::{DateOptions, DateTimePrecision, Field, INDEXED, STORED, STRING, Schema, TEXT},
};

use crate::error::{IndexError, IndexSchemaError};

pub(crate) trait MatrixSearchIndexSchema {
    fn new() -> Self;
    fn default_search_fields(&self) -> Vec<Field>;
    fn primary_key(&self) -> Field;
    fn deletion_key(&self) -> Field;
    fn get_field_name(&self, field: Field) -> &str;
    fn as_tantivy_schema(&self) -> Schema;
    fn make_doc(&self, event: OriginalSyncRoomMessageEvent) -> Result<TantivyDocument, IndexError>;
}

#[derive(Debug, Clone)]
pub(crate) struct RoomMessageSchema {
    inner: Schema,
    /// The event id of this event (primary key).
    event_id_field: Field,
    /// The event id of the event that this event affects.
    /// Used by edits to refer to the event they edited (deletion key).
    original_event_id_field: Field,
    body_field: Field,
    date_field: Field,
    sender_field: Field,
    default_search_fields: Vec<Field>,
}

impl MatrixSearchIndexSchema for RoomMessageSchema {
    fn new() -> Self {
        let mut schema = Schema::builder();
        let event_id_field = schema.add_text_field("event_id", STORED | STRING);
        let original_event_id_field = schema.add_text_field("original_event_id", STRING);
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
            original_event_id_field,
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

    fn deletion_key(&self) -> Field {
        self.original_event_id_field
    }

    fn get_field_name(&self, field: Field) -> &str {
        self.inner.get_field_name(field)
    }

    fn as_tantivy_schema(&self) -> Schema {
        self.inner.clone()
    }

    /// Given an [`OriginalSyncRoomMessageEvent`] return a
    /// [`TantivyDocument`].
    fn make_doc(&self, event: OriginalSyncRoomMessageEvent) -> Result<TantivyDocument, IndexError> {
        let body = match &event.content.msgtype {
            MessageType::Text(content) => Ok(content.body.clone()),
            _ => Err(IndexError::MessageTypeNotSupported),
        }?;

        let mut document = doc!(
            self.event_id_field => event.event_id.to_string(),
            self.body_field => body,
            self.date_field =>
                DateTime::from_timestamp_millis(
                    event.origin_server_ts.get().into()),
            self.sender_field => event.sender.to_string(),
        );

        if let Some(Relation::Replacement(replacement_data)) = &event.content.relates_to {
            document.add_text(self.original_event_id_field, replacement_data.event_id.clone());
        } else {
            document.add_text(self.original_event_id_field, event.event_id);
        }

        Ok(document)
    }
}

impl TryFrom<Schema> for RoomMessageSchema {
    type Error = IndexSchemaError;

    fn try_from(schema: Schema) -> Result<RoomMessageSchema, Self::Error> {
        let event_id_field = schema.get_field("event_id")?;
        let original_event_id_field = schema.get_field("original_event_id")?;
        let body_field = schema.get_field("body")?;
        let date_field = schema.get_field("date")?;
        let sender_field = schema.get_field("sender")?;

        let default_search_fields = vec![body_field];

        Ok(Self {
            inner: schema,
            event_id_field,
            original_event_id_field,
            body_field,
            date_field,
            sender_field,
            default_search_fields,
        })
    }
}
