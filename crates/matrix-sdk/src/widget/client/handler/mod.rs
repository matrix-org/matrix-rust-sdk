//! Client widget API state machine.

use std::sync::Arc;

use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedSender},
    oneshot::Receiver,
};

use self::state::State;
pub(crate) use self::{
    capabilities::Capabilities,
    error::{Error, Result},
    incoming::{
        ErrorResponse as IncomingErrorResponse, Request as IncomingRequest,
        Response as IncomingResponse,
    },
    openid::{OpenIdDecision, OpenIdStatus},
    outgoing::Request as OutgoingRequest,
};
use super::{MatrixDriver, WidgetProxy};
use crate::widget::{
    messages::{
        from_widget::{Action, SupportedApiVersionsResponse as SupportedApiVersions},
        openid::{OpenIdResponse, OpenIdState},
        Header,
    },
    PermissionsProvider,
};

mod capabilities;
mod error;
mod incoming;
mod openid;
mod outgoing;
mod state;

/// A component that processes incoming requests from a widget and generates
/// proper responses. This is essentially a state machine for the client-side
/// widget API.
#[allow(missing_debug_implementations)]
pub(crate) struct MessageHandler {
    /// The processing of the incoming requests is delegated to the worker
    /// (state machine runs in its own task or "thread" if you will), so that
    /// the `handle()` function does not block (originally it was non-async).
    /// This channel allows us sending incoming messages to that worker.
    state_tx: UnboundedSender<IncomingRequest>,
    /// A convenient proxy to the widget that allows us interacting with a
    /// widget via more convenient safely typed high level abstractions.
    widget: Arc<WidgetProxy>,
}

impl MessageHandler {
    /// Creates an instance of a message handler with a given matrix driver
    /// (used to handle all matrix related stuff) and a given widget proxy.
    pub(crate) fn new(
        client: MatrixDriver<impl PermissionsProvider>,
        widget: Arc<WidgetProxy>,
    ) -> Self {
        // Spawn a new task for the state machine. We'll use a channel to delegate
        // handling of messages and other tasks.
        let (state_tx, state_rx) = unbounded_channel();
        tokio::spawn(State::new(widget.clone(), client).listen(state_rx));

        Self { widget, state_tx }
    }

    /// Handles incoming messages from a widget.
    pub(crate) async fn handle(&self, header: Header, action: Action) {
        // Validate the message. Note, that we ignore the error, because the only error
        // that can be returned here is `Err(())`, which means that the widget is
        // disconnected, which does not need to be handled in any way at the moment.
        let _ = match IncomingRequest::new(header, action) {
            // Normally, we send all incoming requests to the worker task (`State::listen`), but the
            // `SupportedApiVersions` request is a special case - not only the widget can send it
            // at any time, but it also may block the processing of other messages until we reply
            // to this one. Luckily, this request is the only single one that does not depend on
            // any state, so we can handle the message right away.
            Ok(IncomingRequest::GetSupportedApiVersion(req)) => {
                self.widget.reply(req.map(Ok(SupportedApiVersions::new()))).await
            }
            // Otherwise, send the incoming request to a worker task. This way our
            // `self.handle()` should actually never block. So the caller can call it many times in
            // a row and it's the `State` (that runs in its own task) that will decide which of
            // them to process sequentially and which in parallel.
            Ok(request) => self.state_tx.send(request).map_err(|_| ()),
            // An error here means that the `header` + `action` pair did not constitute a valid
            // incoming request, so we just report this back to the widget as an error.
            Err(err) => self.widget.reply(err).await,
        };
    }
}
