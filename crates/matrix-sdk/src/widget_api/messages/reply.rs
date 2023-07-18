use std::result::Result as StdResult;

use tokio::sync::oneshot::Sender;

use super::message::{Message, Response};

pub struct Reply<Req, Resp> {
    request: Message<Req, ()>,
    response: Sender<Message<Req, Resp>>,
}

impl <Req, Resp: Clone> Reply<Req, Resp> {
    pub fn new(request: Message<Req, ()>, response: Sender<Message<Req, Resp>>) -> Self {
        Self { request, response }
    }

    pub fn reply(self, response: Resp) -> StdResult<(), Resp> {
        let message = Message {
            header: self.request.header,
            request: self.request.request,
            response: Some(Response::Response(response.clone())),
        };

        self.response.send(message).map_err(|_| response)
    }
}
