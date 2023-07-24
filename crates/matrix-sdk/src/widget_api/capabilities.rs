use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Capabilities {
    navigate: bool,
}

/// A wrapper for the matrix client that only exposes what is available through the capabilities.
pub struct ClientCapabilities {
    pub navigate: Option<Box<dyn Fn(Url) + Send + Sync + 'static>>,
}

impl<'t> From<&'t ClientCapabilities> for Capabilities {
    fn from(capabilities: &'t ClientCapabilities) -> Self {
        Self { navigate: capabilities.navigate.is_some() }
    }
}
