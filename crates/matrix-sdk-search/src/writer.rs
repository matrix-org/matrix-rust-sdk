#![forbid(missing_docs)]

use std::time::Instant;

use tantivy::{IndexWriter, TantivyDocument, TantivyError};

use crate::{
    OpStamp,
    error::{IndexError, IndexWriteError},
    util::{MAX_COMMIT_TIME, MIN_COMMIT_SIZE},
};

pub(crate) struct SearchIndexWriter {
    inner: IndexWriter,
    document_count: usize,
    last_commit_time: Instant,
    last_commit_opstamp: OpStamp,
    // TODO: maybe differentiate a last_stage_opstamp as well
}

impl From<IndexWriter> for SearchIndexWriter {
    fn from(writer: IndexWriter) -> Self {
        SearchIndexWriter {
            last_commit_opstamp: writer.commit_opstamp(),
            last_commit_time: Instant::now(),
            inner: writer,
            document_count: 0,
        }
    }
}

impl SearchIndexWriter {
    pub(crate) fn add_document(&self, document: TantivyDocument) -> Result<OpStamp, IndexError> {
        Ok(self.inner.add_document(document)?)
    }

    pub(crate) fn commit(&mut self) -> Result<OpStamp, IndexWriteError> {
        if (self.document_count >= MIN_COMMIT_SIZE)
            || (Instant::now() - self.last_commit_time > MAX_COMMIT_TIME)
        {
            self.last_commit_opstamp = self.commit_impl()?;
            self.last_commit_time = Instant::now();
            self.document_count = 0;
        } else {
            self.document_count += 1;
        }

        Ok(self.last_commit_opstamp)
    }

    pub(crate) fn force_commit(&mut self) -> Result<OpStamp, IndexError> {
        self.last_commit_opstamp = self.commit_impl()?;
        self.last_commit_time = Instant::now();
        self.document_count = 0;
        Ok(self.last_commit_opstamp)
    }

    fn commit_impl(&mut self) -> Result<OpStamp, TantivyError> {
        self.inner.commit()
    }
}
