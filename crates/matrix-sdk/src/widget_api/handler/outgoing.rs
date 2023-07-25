use super::{super::capabilities::Options, Request};

#[allow(missing_debug_implementations)]
pub enum Message {
    SendMeCapabilities(Request<(), Options>),
    CapabilitiesUpdated(Request<Options, ()>),
}
