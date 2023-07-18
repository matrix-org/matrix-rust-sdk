use async_trait::async_trait;

use super::capabilities::{Capabilities, Request as CapabilitiesRequest};
use crate::Result;

#[async_trait]
pub trait WidgetDriver {
    async fn request_capabilities(&self, capabilities: CapabilitiesRequest) -> Result<Capabilities>;
}
