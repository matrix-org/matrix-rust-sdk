use super::super::capabilities::Capabilities as CapabilityRequest;

pub mod message_types;

pub use self::message_types::ToWidgetMessage;
pub trait ToWidget {
    type Response;
}

pub struct SendMeCapabilities;
impl ToWidget for SendMeCapabilities {
    type Response = CapabilityRequest;
}

pub type CapabilitiesUpdated = CapabilityRequest;
impl ToWidget for CapabilitiesUpdated {
    type Response = ();
}
