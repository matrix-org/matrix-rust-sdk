use serde::{Deserialize, Serialize};
use url::Url;

pub use super::{Error, Request};

#[allow(missing_debug_implementations)]
pub enum Message {
    GetSupportedApiVersion(Request<(), SupportedVersions>),
    ContentLoaded(Request<(), ()>),
    Navigate(Request<Url, Result<(), &'static str>>),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SupportedVersions {
    pub versions: Vec<ApiVersion>,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ApiVersion {
    PreRelease,
}
