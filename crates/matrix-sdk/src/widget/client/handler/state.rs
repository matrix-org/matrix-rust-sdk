use std::sync::Arc;

use tokio::sync::mpsc::UnboundedReceiver;

use super::{
    outgoing, Capabilities, Error, OpenIDResponse, OpenIdStatus, Reply, Result, WidgetProxy,
};
use crate::widget::{
    client::MatrixDriver,
    messages::{
        from_widget::{Action, ApiVersion, SupportedApiVersionsResponse},
        to_widget::{CapabilitiesResponse, CapabilitiesUpdatedRequest},
        Empty, Header, MessageKind as Kind,
    },
    Permissions, PermissionsProvider,
};

pub struct State<W, T> {
    capabilities: Option<Capabilities>,
    widget: Arc<W>,
    client: MatrixDriver<T>,
}

pub enum Task {
    NegotiateCapabilities,
    HandleIncoming(IncomingRequest),
}

#[derive(Debug, Clone)]
pub struct IncomingRequest {
    pub header: Header,
    pub action: Action,
}

impl<W: WidgetProxy, T: PermissionsProvider> State<W, T> {
    pub fn new(widget: Arc<W>, client: MatrixDriver<T>) -> Self {
        Self { capabilities: None, widget, client }
    }

    pub async fn listen(mut self, mut rx: UnboundedReceiver<Task>) {
        while let Some(msg) = rx.recv().await {
            match msg {
                Task::HandleIncoming(req) => {
                    if let Err(_e) = self.handle(req.clone()).await {
                        // TODO: We need to send an error to the widget if the
                        // error was "normal",
                        // i.e. a `Error::Other`. However, a
                        // `WidgetDisconnected` is a terminal
                        // error (we can get it if we try to send a message to
                        // the widget as a response to
                        // the incoming request and the widget's sink is dead at
                        // the moment (widget
                        // disconnected). Such terminal errors don't require any
                        // specific action except for logging (?), use tracing
                        // to log it here later.
                    }
                }
                Task::NegotiateCapabilities => {
                    let _ = self.initialize().await;
                }
            }
        }
    }

    async fn handle(&mut self, IncomingRequest { header, action }: IncomingRequest) -> Result<()> {
        match action {
            Action::ContentLoaded(Kind::Request(req)) => {
                let (response, negotiate) =
                    match (self.widget.init_on_load(), self.capabilities.as_ref()) {
                        (true, None) => (Ok(Empty {}), true),
                        (true, Some(..)) => (Err("Already loaded".into()), false),
                        _ => (Ok(Empty {}), false),
                    };

                self.reply(header, Action::ContentLoaded(req.map(response))).await?;
                if negotiate {
                    self.initialize().await?;
                }
            }

            Action::GetSupportedApiVersion(Kind::Request(req)) => {
                let response = req.map(Ok(SupportedApiVersionsResponse::new()));
                self.reply(header, Action::GetSupportedApiVersion(response)).await?;
            }

            Action::GetOpenID(Kind::Request(req)) => {
                let (reply, handle) = match self.client.get_openid(req.content.clone()) {
                    OpenIdStatus::Resolved(decision) => (decision.into(), None),
                    OpenIdStatus::Pending(handle) => (OpenIDResponse::Pending, Some(handle)),
                };

                let response = req.map(Ok(reply));
                self.reply(header, Action::GetOpenID(response)).await?;
                if let Some(handle) = handle {
                    let status = handle.await.map_err(|_| Error::WidgetDisconnected)?;
                    self.widget
                        .send(outgoing::OpenIDUpdated(status.into()))
                        .await?
                        .map_err(Error::WidgetErrorReply)?;
                }
            }

            Action::ReadEvent(Kind::Request(req)) => {
                let fut = self
                    .caps()?
                    .reader
                    .as_ref()
                    .ok_or(Error::custom("No permissions to read events"))?
                    .read(req.content.clone());
                let response = req.map(Ok(fut.await?));
                self.reply(header, Action::ReadEvent(response)).await?;
            }

            Action::SendEvent(Kind::Request(req)) => {
                let fut = self
                    .caps()?
                    .sender
                    .as_ref()
                    .ok_or(Error::custom("No permissions to send events"))?
                    .send(req.content.clone());
                let response = req.map(Ok(fut.await?));
                self.reply(header, Action::SendEvent(response)).await?;
            }

            _ => {
                // TODO: Widget sent:
                // - A `FromWidget` response instead of sending a request. This
                //   is
                // actually not correct and widgets should not do this. We
                // probably need to send an error in this case,
                // clarify the format of this error later.
                // - A new message kind that we don't support yet. We need to
                //   send an error to the
                // widget informing the widget that the message is not
                // supported.
            }
        }

        Ok(())
    }

    async fn initialize(&mut self) -> Result<()> {
        let CapabilitiesResponse { capabilities: desired } = self
            .widget
            .send(outgoing::CapabilitiesRequest)
            .await?
            .map_err(Error::WidgetErrorReply)?;

        let capabilities = self.client.initialize(desired.clone()).await;
        let approved: Permissions = (&capabilities).into();
        self.capabilities = Some(capabilities);

        let update = CapabilitiesUpdatedRequest { requested: desired, approved };
        self.widget
            .send(outgoing::CapabilitiesUpdate(update))
            .await?
            .map_err(Error::WidgetErrorReply)?;

        Ok(())
    }

    async fn reply(&self, header: Header, action: Action) -> Result<()> {
        self.widget.reply(Reply { header, action }).await
    }

    fn caps(&mut self) -> Result<&mut Capabilities> {
        self.capabilities.as_mut().ok_or(Error::custom("Capabilities have not been negotiated"))
    }
}

impl SupportedApiVersionsResponse {
    pub fn new() -> Self {
        Self {
            versions: vec![
                ApiVersion::V0_0_1,
                ApiVersion::V0_0_2,
                ApiVersion::MSC2762,
                ApiVersion::MSC2871,
                ApiVersion::MSC3819,
            ],
        }
    }
}
