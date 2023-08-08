use std::result::Result as StdResult;

mod api;
mod incoming;
mod outgoing;
mod request;

pub use self::{
    api::{Client, OpenIDState, Widget},
    incoming::Message as Incoming,
    outgoing::OutgoingMessage,
    request::Request,
};
use super::{
    capabilities::{Capabilities, ReadEventRequest},
    messages::{
        capabilities::Options as CapabilitiesReq,
        from_widget::{SendEventRequest, SendEventResponse, SendToDeviceRequest},
        to_widget::CapabilitiesUpdatedRequest as CapabilitiesUpdated,
        MatrixEvent, SupportedVersions, SUPPORTED_API_VERSIONS,
    },
};
pub use super::{Error, Result};

#[allow(missing_debug_implementations)]
pub struct MessageHandler<C, W> {
    capabilities: Option<Capabilities>,
    client: C,
    widget: W,
}

impl<C: Client, W: Widget> MessageHandler<C, W> {
    pub async fn new(client: C, widget: W) -> Result<Self> {
        let mut handler = Self { client, widget, capabilities: None };
        if handler.widget.early_init() {
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

            Incoming::GetOpenID(r) => {
                let state = self.client.get_openid(r.clone()).await;
                r.reply(Ok((&state).into()))?;

                if let OpenIDState::Pending(resolution) = state {
                    let resolved = resolution.await.map_err(|_| Error::WidgetDied)?;
                    self.widget.send(outgoing::OpenIDUpdated(resolved.into())).await?;
                }
            }

            Incoming::ReadEvents(r) => {
                let response = self.read_events(&r).await;
                r.reply(response)?;
            }

            Incoming::SendEvent(r) => {
                let response = self.send_event(&r).await;
                r.reply(response)?;
            }

            Incoming::SendToDeviceRequest(r) => {
                let response = self.send_to_device(&r).await;
                r.reply(response)?;
            }
        }

        Ok(())
    }

    async fn initialise(&mut self) -> Result<()> {
        let requested = self.widget.send(outgoing::SendMeCapabilities).await?;

        let capabilities = self.client.initialise(requested.clone()).await?;
        self.capabilities = Some(capabilities);

        let approved: CapabilitiesReq = self.capabilities.as_ref().unwrap().into();
        self.widget
            .send(outgoing::CapabilitiesUpdated(CapabilitiesUpdated { requested, approved }))
            .await?;

        Ok(())
    }

    async fn read_events(
        &mut self,
        req: &ReadEventRequest,
    ) -> StdResult<Vec<MatrixEvent>, &'static str> {
        self.capabilities()?
            .event_reader
            .as_mut()
            .ok_or("No permissions to read the events")?
            .read(req.clone())
            .await
            .map_err(|_| "Failed to read events")
    }

    async fn send_event(
        &mut self,
        req: &SendEventRequest,
    ) -> StdResult<SendEventResponse, &'static str> {
        self.capabilities()?
            .event_writer
            .as_mut()
            .ok_or("No permissions to write the events")?
            .write(req.clone())
            .await
            .map_err(|_| "Failed to write events")
    }

    async fn send_to_device(&mut self, req: &SendToDeviceRequest) -> StdResult<(), &'static str> {
        self.capabilities()?
            .to_device_sender
            .as_mut()
            .ok_or("No permissions to send to device messages")?
            .send(req.clone())
            .await
            .map_err(|_| "Failed to write events")
    }

    fn capabilities(&mut self) -> StdResult<&mut Capabilities, &'static str> {
        self.capabilities.as_mut().ok_or("Capabilities have not been negotiated")
    }
}
