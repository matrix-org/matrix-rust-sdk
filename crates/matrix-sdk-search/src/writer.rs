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

use tantivy::{IndexWriter, TantivyDocument, TantivyError};

use crate::{OpStamp, error::IndexError};

pub(crate) struct SearchIndexWriter {
    inner: IndexWriter,
    last_commit_opstamp: OpStamp,
}

impl From<IndexWriter> for SearchIndexWriter {
    fn from(writer: IndexWriter) -> Self {
        SearchIndexWriter { last_commit_opstamp: writer.commit_opstamp(), inner: writer }
    }
}

impl SearchIndexWriter {
    pub(crate) fn add_document(&self, document: TantivyDocument) -> Result<OpStamp, IndexError> {
        Ok(self.inner.add_document(document)?) // TODO: This is blocking. Handle
        // it.
    }

    pub(crate) fn commit(&mut self) -> Result<OpStamp, TantivyError> {
        self.last_commit_opstamp = self.inner.commit()?; // TODO: This is blocking. Handle it.
        Ok(self.last_commit_opstamp)
    }
}
