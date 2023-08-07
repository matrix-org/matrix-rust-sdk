use async_trait::async_trait;
use tokio::sync::oneshot::Receiver;

use super::{
    super::messages::{capabilities::Options as CapabilitiesReq, openid},
    capabilities::Capabilities,
    Outgoing, Result,
};

#[async_trait(?Send)]
pub trait Driver {
    async fn initialise(&mut self, req: CapabilitiesReq) -> Result<Capabilities>;
    async fn send(&self, message: Outgoing) -> Result<()>;
    async fn get_openid(&self, req: openid::Request) -> OpenIDState;
}

#[derive(Debug)]
pub enum OpenIDState {
    Resolved(OpenIDResult),
    Pending(Receiver<OpenIDResult>),
}

pub type OpenIDResult = Result<openid::Response>;

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
            Ok(response) => openid::State::Allowed(response),
            Err(_) => openid::State::Blocked,
        }
    }
}
