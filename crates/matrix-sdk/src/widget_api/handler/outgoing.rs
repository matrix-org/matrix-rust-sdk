use crate::widget_api::messages::{capabilities::Options, openid::State as OpenIDState};

use super::Request;

#[allow(missing_debug_implementations)]
pub enum Message {
    SendMeCapabilities(Request<(), Options>),
    CapabilitiesUpdated(Request<Options, ()>),
    OpenIDUpdated(Request<OpenIDState, ()>),
}
