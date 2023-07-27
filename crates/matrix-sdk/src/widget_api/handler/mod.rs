use async_trait::async_trait;

mod incoming;
mod outgoing;
mod request;

pub use self::{incoming::Message as Incoming, outgoing::Message as Outgoing, request::Request};
use super::{
    capabilities::Capabilities,
    messages::{
        capabilities::Options as CapabilitiesReq, SupportedVersions, SUPPORTED_API_VERSIONS,
    },
};
pub use super::{Error, Result};

#[async_trait]
pub trait Driver {
    async fn initialise(&mut self, req: CapabilitiesReq) -> Result<Capabilities>;
    async fn send(&mut self, message: Outgoing) -> Result<()>;
}

#[allow(missing_debug_implementations)]
pub struct MessageHandler<T> {
    capabilities: Option<Capabilities>,
    driver: T,
}

impl<T: Driver> MessageHandler<T> {
    pub fn new(driver: T) -> Self {
        Self { capabilities: None, driver }
    }

    pub async fn handle(&mut self, req: Incoming) -> Result<()> {
        match req {
            Incoming::ContentLoaded(r) => {
                r.reply(())?;
                if self.capabilities.is_none() {
                    return Err(Error::WidgetError("Content loaded twice".to_string()));
                }

                let (req, resp) = Request::new(());
                self.driver.send(Outgoing::SendMeCapabilities(req)).await?;
                let options = resp.await.map_err(|_| Error::WidgetDied)?;

                let capabilities = self.driver.initialise(options).await?;
                self.capabilities = Some(capabilities);

                let approved: CapabilitiesReq = self.capabilities.as_ref().unwrap().into();
                let (req, resp) = Request::new(approved);
                self.driver.send(Outgoing::CapabilitiesUpdated(req)).await?;
                resp.await.map_err(|_| Error::WidgetDied)?;
            }

            Incoming::GetSupportedApiVersion(r) => {
                r.reply(SupportedVersions { versions: SUPPORTED_API_VERSIONS.to_vec() })?;
            }
        }

        Ok(())
    }
}
