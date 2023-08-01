use std::{ops::Deref, result::Result as StdResult};

use tokio::sync::oneshot::{Receiver, Sender};

use super::{Error, Result};

type Response<T> = StdResult<T, &'static str>;

#[allow(missing_debug_implementations)]
pub struct Request<Req, Resp> {
    content: Req,
    reply: Sender<Response<Resp>>,
}

impl<C, R> Request<C, R> {
    pub fn new(content: C) -> (Self, ResponseReceiver<R>) {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        (Self { content, reply }, ResponseReceiver { receiver })
    }

    pub fn reply(self, response: Response<R>) -> Result<()> {
        self.reply.send(response).map_err(|_| Error::WidgetDied)
    }
}

impl<Req, Resp> Deref for Request<Req, Resp> {
    type Target = Req;

    fn deref(&self) -> &Self::Target {
        &self.content
    }
}

#[allow(missing_debug_implementations)]
pub struct ResponseReceiver<T> {
    receiver: Receiver<Response<T>>,
}

impl<T> ResponseReceiver<T> {
    pub async fn recv(self) -> Result<T> {
        self.receiver
            .await
            .map_err(|_| Error::WidgetDied)?
            .map_err(|e| Error::WidgetError(e.to_string()))
    }
}
