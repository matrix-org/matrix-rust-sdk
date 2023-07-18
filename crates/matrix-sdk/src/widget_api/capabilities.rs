use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{Receiver, Sender};
use url::Url;

use crate::Result;
use super::openid::OpenIDCredentials;

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Request {
    read: bool,
    send: bool,
    navigate: bool,
    open_id_auth: bool,
}

pub struct Capabilities {
    pub navigate: Option<NavigateFunc>,
    pub events: Option<Receiver<Event>>,
    pub send: Option<Sender<Event>>,
    pub acquire_token: Option<OpenIDFunc>,
}

#[derive(Debug)]
pub enum Event {
    RoomEvent,
    ToDeviceMessage,
}

type CapabilityFuture<T> = BoxFuture<'static, Result<T>>;
type NavigateFunc = Box<dyn Fn(Url) -> CapabilityFuture<()>>;
type OpenIDFunc = Box<dyn Fn() -> CapabilityFuture<OpenIDCredentials>>;
