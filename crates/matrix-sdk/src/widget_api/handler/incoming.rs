use crate::widget_api::messages::SupportedVersions;

pub use super::{Error, Request};

#[allow(missing_debug_implementations)]
pub enum Message {
    GetSupportedApiVersion(Request<(), SupportedVersions>),
    ContentLoaded(Request<(), ()>),
}

