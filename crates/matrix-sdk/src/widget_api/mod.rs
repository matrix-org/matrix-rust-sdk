// TODO: Remove this supress once we're ready to write the documentation.
#![allow(missing_docs)]

pub mod api;
pub mod error;
pub mod handler;
pub mod matrix;
pub mod messages;

pub use self::error::{Error, Result};

pub fn run_client_widget_api() {
    println!("Hello, world!");
}
