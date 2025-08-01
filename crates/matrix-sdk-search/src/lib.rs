pub type OpStamp = u64;

mod schema;
mod util;
mod writer;

/// A module for errors relating to the search crate.
pub mod error;
/// A module for the search index.
pub mod index;
/// A module for testing the search crate.
pub mod testing;
