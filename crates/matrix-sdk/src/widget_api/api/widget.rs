use async_trait::async_trait;
use tokio::sync::{
    mpsc::{UnboundedReceiver as Receiver, UnboundedSender as Sender},
    oneshot,
};
use uuid::Uuid;

use super::{
    super::{
        handler::{OutgoingMessage, Widget as WidgetSender},
        messages::{to_widget::ToWidgetMessage, Header},
        Error,
    },
    PendingResponses, Result,
};

#[derive(Debug)]
pub struct Widget {
    pub info: Info,
    pub comm: Comm,
}

#[derive(Debug)]
pub struct Info {
    pub id: String,
    pub early_init: bool,
}

#[derive(Debug)]
pub struct Comm {
    pub from: Receiver<String>,
    pub to: Sender<String>,
}

impl Comm {
    pub fn sink(&self) -> Sender<String> {
        self.to.clone()
    }

    pub async fn recv(&mut self) -> Option<String> {
        self.from.recv().await
    }
}

pub(super) struct WidgetSink {
    info: Info,
    sink: Sender<ToWidgetMessage>,
    pending: PendingResponses,
}

impl WidgetSink {
    pub(super) fn new(
        info: Info,
        sink: Sender<ToWidgetMessage>,
        pending: PendingResponses,
    ) -> Self {
        Self { info, sink, pending }
    }
}

#[async_trait]
impl WidgetSender for WidgetSink {
    async fn send<M: OutgoingMessage>(&self, msg: M) -> Result<M::Response> {
        let id = Uuid::new_v4();
        let request_id = id.to_string();
        let raw = msg.into_message(Header { request_id, widget_id: self.info.id.clone() });
        self.sink.send(raw).map_err(|_| Error::WidgetDied)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("Pending mutex poisoned").insert(id.to_string(), tx);
        Ok(M::extract_response(rx.await.map_err(|_| Error::WidgetDied)?)?)
    }

    fn early_init(&self) -> bool {
        self.info.early_init
    }
}
