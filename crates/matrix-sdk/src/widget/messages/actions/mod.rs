use serde::{Deserialize, Serialize};

pub mod from_widget;
mod message;
pub mod to_widget;

pub use self::message::{Empty, Kind as MessageKind, Request, Response};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "api")]
pub enum Action {
    FromWidget(from_widget::Action),
    ToWidget(to_widget::Action),
}
