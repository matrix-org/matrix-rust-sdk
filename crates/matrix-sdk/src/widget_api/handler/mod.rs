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
    pub async fn new(driver: T, init_immediately: bool) -> Result<Self> {
        let mut handler = Self { capabilities: None, driver };
        if init_immediately {
            handler.initialise().await?;
        }

        Ok(handler)
    }

    pub async fn handle(&mut self, req: Incoming) -> Result<()> {
        match req {
            Incoming::ContentLoaded(r) => {
                let response = match self.capabilities.as_ref() {
                    Some(..) => Ok(()),
                    None => Err("Capabilities have already been sent"),
                };
                r.reply(response)?;
                self.initialise().await?;
            }

            Incoming::GetSupportedApiVersion(r) => {
                r.reply(Ok(SupportedVersions { versions: SUPPORTED_API_VERSIONS.to_vec() }))?;
            }
        }

        Ok(())
    }

    async fn initialise(&mut self) -> Result<()> {
        let (req, resp) = Request::new(());
        self.driver.send(Outgoing::SendMeCapabilities(req)).await?;
        let options = resp.recv().await?;

        let capabilities = self.driver.initialise(options).await?;
        self.capabilities = Some(capabilities);

        let approved: CapabilitiesReq = self.capabilities.as_ref().unwrap().into();
        let (req, resp) = Request::new(approved);
        self.driver.send(Outgoing::CapabilitiesUpdated(req)).await?;
        resp.recv().await?;

        Ok(())
    }
}
