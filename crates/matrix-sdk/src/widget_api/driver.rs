use async_trait::async_trait;

use crate::Result;
use super::capabilities::{Request, Capabilities};

#[async_trait]
pub trait WidgetDriver {
    async fn request_capabilities(&self, capabilities: Request) -> Result<Capabilities>;
}
