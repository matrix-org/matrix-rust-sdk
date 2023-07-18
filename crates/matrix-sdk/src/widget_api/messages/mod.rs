use super::{capabilities::Request as CapabilityRequest, openid::OpenIDCredentials};

mod message;
mod reply;

pub struct Request<C, R> {
    content: C,
    reply: reply::Reply<C, R>,
}

pub enum Incoming {
    GetSupportedApiVersion(Request<(), SupportedVersions>),
    ContentLoaded(Request<(), ()>),
    PermissionRequest(Request<CapabilityRequest, CapabilityRequest>),
    GetOpenIdCredentials(Request<(), OpenIDCredentials>),
    OpenModalWidget(Request<(), ()>),
    SendEvent(Request<(), ()>),
    SendToDeviceMessage(Request<(), ()>),
    ReadEvents(Request<(), ()>),
}

pub struct SupportedVersions {
    versions: Vec<ApiVersion>,
}

pub enum ApiVersion {
    PreRelease,
    Release,
    MSCWhatever,
}
