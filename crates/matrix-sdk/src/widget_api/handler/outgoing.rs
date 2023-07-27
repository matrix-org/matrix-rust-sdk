use crate::widget_api::messages;

use super::Request;

#[allow(missing_debug_implementations)]
pub enum Message {
    SendMeCapabilities(Request<(), messages::capabilities::Options>),
    CapabilitiesUpdated(Request<messages::capabilities::Options, ()>),
}
