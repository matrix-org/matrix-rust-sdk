use async_trait::async_trait;
use tokio::sync::oneshot::Receiver;

use super::{
    super::{
        capabilities::Capabilities,
        messages::{capabilities::Options as CapabilitiesReq, openid},
    },
    OutgoingMessage, Result,
};

#[async_trait]
pub trait Driver {
    async fn initialise(&self, req: CapabilitiesReq) -> Result<Capabilities>;
    async fn send<T: OutgoingMessage>(&self, message: T) -> Result<T::Response>;
    async fn get_openid(&self, req: openid::Request) -> OpenIDState;
}

#[derive(Debug)]
pub enum OpenIDState {
    Resolved(OpenIDResult),
    Pending(Receiver<OpenIDResult>),
}

pub type OpenIDResult = Option<openid::Response>;

impl<'t> From<&'t OpenIDState> for openid::State {
    fn from(state: &'t OpenIDState) -> Self {
        match state {
            OpenIDState::Resolved(resolved) => resolved.clone().into(),
            OpenIDState::Pending(..) => openid::State::Pending,
        }
    }
}

impl From<OpenIDResult> for openid::State {
    fn from(result: OpenIDResult) -> Self {
        match result {
            Some(response) => openid::State::Allowed(response),
            None => openid::State::Blocked,
        }
    }
}
