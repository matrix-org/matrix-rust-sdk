use tokio::sync::{
    mpsc::{Receiver, Sender},
    oneshot::Sender as Reply,
};

use super::capabilities::Request as CapRequest;

pub struct Widget {
    pub id: String,
    pub incoming: Receiver<Incoming>, // widget -> client
    pub outgoing: Sender<Outgoing>,   // client -> widget
}

pub struct Message<C, R> {
    content: C,
    reply: Reply<R>,
}

// TODO: Replace with a data types from @toger5.
pub enum Incoming {
    ContentLoaded(Message<(), ()>),
    PermissionRequest(Message<CapRequest, CapRequest>),
}

// TODO: Replace with a data types from @toger5.
pub enum Outgoing {}
