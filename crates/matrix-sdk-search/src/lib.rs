#![doc = include_str!("../README.md")]
#![forbid(missing_docs)]

/// Monotonically increasing timestamp of operations on the index.
pub type OpStamp = u64;

pub(crate) const TANTIVY_INDEX_MEMORY_BUDGET: usize = 50_000_000;

mod encrypted;
mod schema;
mod writer;

/// A module for errors relating to the search crate.
pub mod error;
/// A module for the search index.
pub mod index;
