use std::sync::Arc;

use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedSender},
    oneshot::Receiver,
};

use self::state::{State, Task as StateTask};
pub(crate) use self::{
    capabilities::Capabilities,
    error::{Error, Result},
    openid::{OpenIdDecision, OpenIdStatus},
    outgoing::{Request as Outgoing, Response},
    state::IncomingRequest,
};
use super::{MatrixDriver, WidgetProxy};
use crate::widget::{
    messages::{
        from_widget::{Action, SupportedApiVersionsResponse},
        Header, MessageKind, OpenIdResponse, OpenIdState,
    },
    PermissionsProvider,
};

mod capabilities;
mod error;
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

    pub(crate) async fn handle(&self, req: IncomingRequest) -> Result<()> {
        match req.action {
            Action::GetSupportedApiVersion(MessageKind::Request(r)) => {
                let response = r.map(Ok(SupportedApiVersionsResponse::new()));
                self.widget
                    .reply(Reply {
                        header: req.header,
                        action: Action::GetSupportedApiVersion(response),
                    })
                    .await?;
            }

            _ => {
                self.state_tx
                    .send(StateTask::HandleIncoming(req))
                    .map_err(|_| Error::WidgetDisconnected)?;
            }
        }

        Ok(())
    }
}

pub(crate) struct Reply {
    pub(crate) header: Header,
    // TODO: Define a new type, so that we can guarantee on compile time that we can only send
    // `Action(Kind::Response)` here and not `Action(Kind::Request)`.
    pub(crate) action: Action,
}
