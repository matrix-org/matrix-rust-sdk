use std::sync::Arc;

use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{info, warn};

use super::{
    outgoing, Capabilities, Error, IncomingRequest as Request, OpenIdResponse, OpenIdStatus, Result,
};
use crate::widget::{
    client::{MatrixDriver, WidgetProxy},
    messages::{
        from_widget::{ApiVersion, SupportedApiVersionsResponse},
        to_widget::{CapabilitiesResponse, CapabilitiesUpdatedRequest},
        Empty,
    },
    Permissions, PermissionsProvider,
};

pub(super) struct State<T> {
    capabilities: Option<Capabilities>,
    widget: Arc<WidgetProxy>,
    client: MatrixDriver<T>,
}

pub(crate) enum Task {
    NegotiateCapabilities,
    HandleIncoming(Request),
}

impl<T: PermissionsProvider> State<T> {
    pub(super) fn new(widget: Arc<WidgetProxy>, client: MatrixDriver<T>) -> Self {
        Self { capabilities: None, widget, client }
    }

    pub(super) async fn listen(mut self, mut rx: UnboundedReceiver<Task>) {
        while let Some(msg) = rx.recv().await {
            match msg {
                Task::HandleIncoming(request) => {
                    if let Err(err) = self.handle(request.clone()).await {
                        if let Err(..) = self.widget.reply(request.fail(err.to_string())).await {
                            info!("Dropped reply, widget is disconnected");
                            break;
                        }
                    }
                }
                Task::NegotiateCapabilities => {
                    if let Err(err) = self.initialize().await {
                        // We really don't have a mechanism to inform a widget about out of bound
                        // errors. So the only thing we can do here is to log it.
                        warn!(error = %err, "Failed to initialize widget");
                        break;
                    }
                }
            }
        }
    }

    async fn handle(&mut self, request: Request) -> Result<()> {
        match request {
            Request::GetSupportedApiVersion(req) => {
                let _ = self.widget.reply(req.map(Ok(SupportedApiVersionsResponse::new())));
            }

            Request::ContentLoaded(req) => {
                let (response, negotiate) =
                    match (self.widget.init_on_load(), self.capabilities.as_ref()) {
                        (true, None) => (Ok(Empty {}), true),
                        (true, Some(..)) => (Err("Already loaded".into()), false),
                        _ => (Ok(Empty {}), false),
                    };

                let _ = self.widget.reply(req.map(response)).await;
                if negotiate {
                    self.initialize().await?;
                }
            }

            Request::GetOpenId(req) => {
                let (reply, handle) = match self.client.get_openid((*req).clone()) {
                    OpenIdStatus::Resolved(decision) => (decision.into(), None),
                    OpenIdStatus::Pending(handle) => (OpenIdResponse::Pending, Some(handle)),
                };

                let _ = self.widget.reply(req.map(Ok(reply)));
                if let Some(handle) = handle {
                    let status = handle.await.map_err(|_| Error::WidgetDisconnected)?;
                    self.widget
                        .send(outgoing::OpenIdUpdated(status.into()))
                        .await?
                        .map_err(Error::WidgetErrorReply)?;
                }
            }

            Request::ReadEvent(req) => {
                let fut = self
                    .caps()?
                    .reader
                    .as_ref()
                    .ok_or(Error::custom("No permissions to read events"))?
                    .read((*req).clone());
                let resp = Ok(fut.await?);
                let _ = self.widget.reply(req.map(resp)).await;
            }

            Request::SendEvent(req) => {
                let fut = self
                    .caps()?
                    .sender
                    .as_ref()
                    .ok_or(Error::custom("No permissions to send events"))?
                    .send((*req).clone());
                let resp = Ok(fut.await?);
                let _ = self.widget.reply(req.map(resp)).await;
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

    fn caps(&mut self) -> Result<&mut Capabilities> {
        self.capabilities.as_mut().ok_or(Error::custom("Capabilities have not been negotiated"))
    }
}

impl SupportedApiVersionsResponse {
    pub(crate) fn new() -> Self {
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
