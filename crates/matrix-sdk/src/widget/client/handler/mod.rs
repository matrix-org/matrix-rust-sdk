use std::sync::Arc;

use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedSender},
    oneshot::Receiver,
};

use self::state::{State, Task as StateTask};
pub(crate) use self::{
    capabilities::Capabilities,
    error::{Error, Result},
    incoming::{Request as IncomingRequest, Response as IncomingResponse},
    openid::{OpenIdDecision, OpenIdStatus},
    outgoing::{Request as OutgoingRequest, Response as OutgoingResponse},
};
use super::{MatrixDriver, WidgetProxy};
use crate::widget::{
    messages::{
        from_widget::{Action, SupportedApiVersionsResponse},
        Header, OpenIdResponse, OpenIdState,
    },
    PermissionsProvider,
};

mod capabilities;
mod error;
mod incoming;
mod openid;
mod outgoing;
mod state;

#[allow(missing_debug_implementations)]
pub(crate) struct MessageHandler {
    state_tx: UnboundedSender<StateTask>,
    widget: Arc<WidgetProxy>,
}

impl MessageHandler {
    pub(crate) fn new(client: MatrixDriver<impl PermissionsProvider>, widget: WidgetProxy) -> Self {
        let widget = Arc::new(widget);

        let (state_tx, state_rx) = unbounded_channel();
        tokio::spawn(State::new(widget.clone(), client).listen(state_rx));

        if !widget.init_on_load() {
            let _ = state_tx.send(StateTask::NegotiateCapabilities);
        }

        Self { widget, state_tx }
    }

    pub(crate) async fn handle(&self, header: Header, action: Action) -> Result<()> {
        match IncomingRequest::new(header, action).ok_or(Error::custom("Invalid message"))? {
            IncomingRequest::GetSupportedApiVersion(req) => self
                .widget
                .reply(req.map(Ok(SupportedApiVersionsResponse::new())))
                .await
                .map_err(|_| Error::WidgetDisconnected),
            request => self
                .state_tx
                .send(StateTask::HandleIncoming(request))
                .map_err(|_| Error::WidgetDisconnected),
        }
    }
}
