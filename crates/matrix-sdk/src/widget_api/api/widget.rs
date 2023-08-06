use async_trait::async_trait;
use tokio::sync::mpsc::{UnboundedReceiver as Receiver, UnboundedSender as Sender};

use super::super::{
    capabilities::Capabilities,
    handler::OpenIDState,
    messages::{capabilities::Options, openid::Request as OpenIDRequest},
    Result,
};

#[derive(Debug)]
pub struct Info {
    pub id: String,
    pub negotiate: bool,
}

#[async_trait]
pub trait Api: Send + Sync + 'static {
    async fn initialise(&self, req: Options) -> Result<Capabilities>;
    async fn get_openid(&self, req: OpenIDRequest) -> OpenIDState;
}

#[derive(Debug)]
pub struct Widget<T> {
    pub info: Info,
    pub api: T,
    pub comm: Comm,
}

#[derive(Debug)]
pub struct Comm {
    from: Receiver<String>,
    to: Sender<String>,
}

impl Comm {
    pub fn sink(&self) -> Sender<String> {
        self.to.clone()
    }

    pub async fn recv(&mut self) -> Option<String> {
        self.from.recv().await
    }
}
