#![forbid(missing_docs)]

use std::time::Duration;

pub(crate) const TANTIVY_INDEX_MEMORY_BUDGET: usize = 50_000_000;
pub(crate) const MIN_COMMIT_SIZE: usize = 500;
pub(crate) const MAX_COMMIT_TIME: Duration = Duration::from_secs(5);
