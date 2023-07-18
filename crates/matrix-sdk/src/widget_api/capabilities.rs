use std::time::Instant;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{Receiver, Sender};
use url::Url;

use crate::Result;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Request {
    read: bool,
    send: bool,
    navigate: bool,
    open_id_auth: bool,
}

type CapabilityFuture<T> = BoxFuture<'static, Result<T>>;
type NavigateFunc = Box<dyn Fn(Url) -> CapabilityFuture<()>>;
type OpenIDFunc = Box<dyn Fn() -> CapabilityFuture<OpenIDCredentials>>;

pub struct Capabilities {
    pub navigate: Option<NavigateFunc>,
    pub events: Option<Receiver<Event>>,
    pub send: Option<Sender<Event>>,
    pub acquire_token: Option<OpenIDFunc>,
}

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

// TODO: Replace it with the actual events that we can get from Matrix.
#[derive(Debug)]
pub enum Event {
    RoomEvent,
    ToDeviceMessage,
}
