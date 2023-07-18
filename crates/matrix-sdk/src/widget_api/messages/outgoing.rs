use super::super::capabilities::Options as CapabilityRequest;

pub trait Outgoing {
    type Response;
}

pub struct SendMeCapabilities;
impl Outgoing for SendMeCapabilities {
    type Response = CapabilityRequest;
}

pub type CapabilitiesUpdated = CapabilityRequest;
impl Outgoing for CapabilitiesUpdated {
    type Response = ();
}
