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

use ruma::EventId;
use tantivy::{IndexWriter, TantivyDocument, TantivyError, Term};

use crate::{
    OpStamp,
    error::IndexError,
    schema::{MatrixSearchIndexSchema, RoomMessageSchema},
};

pub(crate) struct SearchIndexWriter {
    inner: IndexWriter,
    last_commit_opstamp: OpStamp,
    schema: RoomMessageSchema,
}

impl SearchIndexWriter {
    pub(crate) fn new(writer: IndexWriter, schema: RoomMessageSchema) -> Self {
        Self { last_commit_opstamp: writer.commit_opstamp(), inner: writer, schema }
    }

    pub(crate) fn add(&self, document: TantivyDocument) -> Result<OpStamp, IndexError> {
        Ok(self.inner.add_document(document)?) // TODO: This is blocking. Handle
        // it.
    }

    pub(crate) fn remove(&self, event_id: &EventId) {
        self.inner
            .delete_term(Term::from_field_text(self.schema.deletion_key(), event_id.as_str()));
    }

    pub(crate) fn commit(&mut self) -> Result<OpStamp, TantivyError> {
        self.last_commit_opstamp = self.inner.commit()?; // TODO: This is blocking. Handle it.
        Ok(self.last_commit_opstamp)
    }
}
