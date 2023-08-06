use async_trait::async_trait;
use tokio::sync::{mpsc::UnboundedSender as Sender, oneshot};
use uuid::Uuid;

use super::{
    super::{
        capabilities::Capabilities,
        handler::{Driver as HandlerDriver, OpenIDState, OutgoingMessage},
        messages::{
            capabilities::Options, openid::Request as OpenIDRequest, to_widget::ToWidgetMessage,
            Header,
        },
        Error,
    },
    widget::Api as WidgetApi,
    PendingResponses, Result,
};

pub struct Driver<T> {
    id: String,
    api: T,
    sink: Sender<ToWidgetMessage>,
    pending: PendingResponses,
}

impl<T> Driver<T> {
    pub fn new(
        id: String,
        api: T,
        sink: Sender<ToWidgetMessage>,
        pending: PendingResponses,
    ) -> Self {
        Self { id, api, sink, pending }
    }
}

#[async_trait]
impl<T: WidgetApi> HandlerDriver for Driver<T> {
    async fn initialise(&self, req: Options) -> Result<Capabilities> {
        self.api.initialise(req).await
    }

    async fn get_openid(&self, req: OpenIDRequest) -> OpenIDState {
        self.api.get_openid(req).await
    }

    async fn send<M: OutgoingMessage>(&self, msg: M) -> Result<M::Response> {
        let id = Uuid::new_v4();
        let request_id = id.to_string();
        let raw = msg.into_message(Header { request_id, widget_id: self.id.clone() });
        self.sink.send(raw).map_err(|_| Error::WidgetDied)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("Pending mutex poisoned").insert(id.to_string(), tx);
        Ok(M::extract_response(rx.await.map_err(|_| Error::WidgetDied)?)?)
    }
}
