use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct Options {
    navigate: bool,
}

#[allow(missing_debug_implementations)]
pub struct Capabilities {
    pub navigate: Option<Box<dyn Fn(Url) + Send + Sync + 'static>>,
}

impl<'t> From<&'t Capabilities> for Options {
    fn from(capabilities: &'t Capabilities) -> Self {
        Self { navigate: capabilities.navigate.is_some() }
    }
}
