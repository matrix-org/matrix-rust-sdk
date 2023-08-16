mod api;
mod capabilities;
mod incoming;
mod outgoing;
mod request;

pub use self::{
    api::{Client, OpenIDState, Widget},
    capabilities::{Capabilities, EventReader, EventSender, Filtered},
    incoming::Message as Incoming,
    outgoing::OutgoingMessage,
    request::Request,
};

use super::{
    messages::{
        capabilities::Options as CapabilitiesReq,
        from_widget::{ReadEventRequest, ReadEventResponse, SendEventRequest, SendEventResponse},
        to_widget::CapabilitiesUpdatedRequest as CapabilitiesUpdated,
        SupportedVersions, SUPPORTED_API_VERSIONS,
    },
    Error, Result,
};

#[allow(missing_debug_implementations)]
pub struct MessageHandler<C, W> {
    capabilities: Option<Capabilities>,
    client: C,
    widget: W,
}

impl<C: Client, W: Widget> MessageHandler<C, W> {
    pub async fn new(client: C, widget: W) -> Result<Self> {
        let mut handler = Self { client, widget, capabilities: None };
        if !handler.widget.init_on_load() {
            handler.initialise().await?;
        }

        Ok(handler)
    }

    pub async fn handle(&mut self, req: Incoming) -> Result<()> {
        match req {
            Incoming::ContentLoaded(r) => {
                let (response, negotiate) =
                    match (self.widget.init_on_load(), self.capabilities.as_ref()) {
                        (true, None) => (Ok(()), true),
                        (true, Some(..)) => (Err(Error::AlreadyLoaded), false),
                        _ => (Ok(()), false),
                    };

                r.reply(response)?;
                if negotiate {
                    self.initialise().await?;
                }
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
                let response = self.read_events(&*r).await;
                r.reply(response)?;
            }

            Incoming::SendEvent(r) => {
                let response = self.send_event(&*r).await;
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
        let update = CapabilitiesUpdated { requested, approved };
        self.widget.send(outgoing::CapabilitiesUpdated(update)).await?;

        Ok(())
    }

    async fn read_events(&mut self, req: &ReadEventRequest) -> Result<ReadEventResponse> {
        let fut = self.caps()?.reader.as_ref().ok_or(Error::InvalidPermissions)?.read(req.clone());
        fut.await
    }

    async fn send_event(&mut self, req: &SendEventRequest) -> Result<SendEventResponse> {
        let fut = self.caps()?.sender.as_ref().ok_or(Error::InvalidPermissions)?.send(req.clone());
        fut.await
    }

    fn caps(&mut self) -> Result<&mut Capabilities> {
        self.capabilities.as_mut().ok_or(Error::InvalidPermissions)
    }
}
