use std::ops::Deref;

use tokio::sync::oneshot::{Receiver, Sender};

use super::{Error, Result};

#[allow(missing_debug_implementations)]
pub struct Request<Req, Resp> {
    content: Req,
    reply: Sender<Resp>,
}

impl<C, R> Request<C, R> {
    pub fn new(content: C) -> (Self, Receiver<R>) {
        let (reply, response) = tokio::sync::oneshot::channel();
        (Self { content, reply }, response)
    }

    pub fn reply(self, response: R) -> Result<()> {
        self.reply.send(response).map_err(|_| Error::WidgetDied)
    }
}

impl<Req, Resp> Deref for Request<Req, Resp> {
    type Target = Req;

    fn deref(&self) -> &Self::Target {
        &self.content
    }
}
