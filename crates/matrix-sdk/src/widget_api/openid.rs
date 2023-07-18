use std::time::Instant;

use url::Url;

#[derive(Debug)]
pub struct OpenIDCredentials {
    pub token: String,
    pub kind: TokenKind,
    pub expires: Instant,
    pub homeserver: Url,
}

#[derive(Debug)]
pub enum TokenKind {
    Bearer,
    Custom(String),
}
