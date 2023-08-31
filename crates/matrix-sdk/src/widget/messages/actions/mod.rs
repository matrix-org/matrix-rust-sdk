use serde::{Deserialize, Serialize};

pub mod from_widget;
mod message;
pub mod to_widget;

pub use self::message::{Empty, Kind as MessageKind, Request, Response, ResponseBody};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "api")]
#[serde(rename_all = "camelCase")]
pub(crate) enum Action {
    FromWidget(from_widget::Action),
    ToWidget(to_widget::Action),
}
