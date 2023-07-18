use url::Url;

use super::Request;

pub enum Incoming {
    GetSupportedApiVersion(Request<(), SupportedVersions>),
    ContentLoaded(Request<(), ()>),
    Navigate(Request<Url, Result<(), &'static str>>),
}

pub struct SupportedVersions {
    pub versions: Vec<ApiVersion>,
}
pub enum ApiVersion {
    PreRelease,
}
