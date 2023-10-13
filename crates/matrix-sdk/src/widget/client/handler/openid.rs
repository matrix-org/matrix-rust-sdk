//! OpenID types that could be used when requesting OIDC tokens.

use super::{OpenIdResponse, OpenIdState, Receiver};

#[derive(Debug)]
pub(crate) enum OpenIdStatus {
    #[allow(dead_code)]
    Resolved(OpenIdDecision),
    Pending(Receiver<OpenIdDecision>),
}

#[derive(Debug, Clone)]
pub(crate) enum OpenIdDecision {
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
