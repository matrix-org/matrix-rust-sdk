use async_trait::async_trait;

use self::{
    messages::{Incoming, Outgoing, SupportedVersions, ApiVersion, SendMeCapabilities, CapabilitiesUpdated},
    capabilities::{Options as CapabilitiesReq, Capabilities},
};
pub use self::error::{Error, Result};

pub mod capabilities;
pub mod messages;
pub mod error;

#[async_trait]
pub trait WidgetDriver {
    async fn initialise(&mut self, req: CapabilitiesReq) -> Result<Capabilities>;
    async fn send<T: Outgoing>(&mut self, req: T) -> Result<T::Response>;
}

pub struct ClientApiHandler<T> {
    capabilities: Option<Capabilities>,
    driver: T,
}

impl<T: WidgetDriver> ClientApiHandler<T> {
    pub fn new(driver: T) -> Self {
        Self {
            capabilities: None,
            driver,
        }
    }

    pub async fn handle(&mut self, req: Incoming) -> Result<()> {
        match req {
            Incoming::ContentLoaded(r) => {
                r.reply(())?;
                if self.capabilities.is_none() {
                    return Err(Error::WidgetError("Content loaded twice".to_string()));
                }

                let requested = self.driver.send(SendMeCapabilities).await?;
                let capabilities = self.driver.initialise(requested).await?;
                self.capabilities = Some(capabilities);

                let approved: CapabilitiesUpdated = self.capabilities.as_ref().unwrap().into();
                self.driver.send(approved).await?;
            }

            Incoming::GetSupportedApiVersion(r) => {
                r.reply(SupportedVersions {
                    versions: vec![ApiVersion::PreRelease],
                })?;
            }

            Incoming::Navigate(r) => {
                match self.capabilities.as_ref().and_then(|c| c.navigate.as_ref()) {
                    Some(navigate) => {
                        navigate(r.content.clone());
                        r.reply(Ok(()))?;
                    }
                    None => {
                        r.reply(Err("Not permissions to call navigate"))?;
                    }
                }
            }
        }

        Ok(())
    }
}
