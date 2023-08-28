use std::sync::Arc;

use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedSender},
    oneshot::Receiver,
};

pub(crate) use self::state::IncomingRequest;
use self::state::{State, Task as StateTask};
pub use self::{
    capabilities::{Capabilities, EventReader, EventSender, Filtered},
    error::{Error, Result},
    openid::{OpenIdDecision, OpenIdStatus},
    outgoing::{Request as Outgoing, Response},
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
mod outgoing;
mod state;

#[allow(missing_debug_implementations)]
pub struct MessageHandler {
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

pub struct Reply {
    pub header: Header,
    // TODO: Define a new type, so that we can guarantee on compile time that we can only send
    // `Action(Kind::Response)` here and not `Action(Kind::Request)`.
    pub action: Action,
}

mod openid {
    use super::{OpenIdResponse, OpenIdState, Receiver};

    #[derive(Debug)]
    pub enum OpenIdStatus {
        #[allow(dead_code)]
        Resolved(OpenIdDecision),
        Pending(Receiver<OpenIdDecision>),
    }

    #[derive(Debug, Clone)]
    pub enum OpenIdDecision {
        Blocked,
        Allowed(OpenIdState),
    }

    impl From<OpenIdDecision> for OpenIdResponse {
        fn from(decision: OpenIdDecision) -> Self {
            match decision {
                OpenIdDecision::Allowed(resolved) => OpenIdResponse::Allowed(resolved),
                OpenIdDecision::Blocked => OpenIdResponse::Blocked,
            }
        }
    }
}
