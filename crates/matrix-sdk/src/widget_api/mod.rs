// TODO: Remove this supress once we're ready to write the documentation.
#![allow(missing_docs)]

pub mod api;
pub mod capabilities;
pub mod error;
pub mod handler;
pub mod messages;

pub use self::error::{Error, Result};
