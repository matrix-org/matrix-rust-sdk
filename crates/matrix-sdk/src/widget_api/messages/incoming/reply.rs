use tokio::sync::oneshot::Sender;

use super::super::message::{Message, Response};

pub struct Reply<Req, Resp> {
    request: Message<Req, ()>,
    response: Sender<Message<Req, Resp>>,
}

impl<Req, Resp> Reply<Req, Resp> {
    pub fn new(request: Message<Req, ()>, response: Sender<Message<Req, Resp>>) -> Self {
        Self { request, response }
    }

    pub fn reply(self, response: Resp) -> Result<(), Resp> {
        let message = Message {
            widget_id: self.request.widget_id,
            request_id: self.request.request_id,
            request: self.request.request,
            response: Some(Response::Response(response)),
        };

        self.response.send(message).map_err(|r| {
            // Safe to unwrap here, because the `response` is always `Some()` (see above).
            let result: Result<Resp, _> = r.response.unwrap().into();
            result.unwrap()
        })
    }
}
