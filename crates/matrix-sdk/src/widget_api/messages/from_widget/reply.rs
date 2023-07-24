use tokio::sync::oneshot::Sender;

use super::super::message::{MessageBody, Response};

pub struct Reply<Req, Resp> {
    request: MessageBody<Req, ()>,
    response: Sender<MessageBody<Req, Resp>>,
}

impl<Req, Resp> Reply<Req, Resp> {
    pub fn new(request: MessageBody<Req, ()>, response: Sender<MessageBody<Req, Resp>>) -> Self {
        Self { request, response }
    }

    pub fn reply(self, response: Resp) -> Result<(), Resp> {
        let message = MessageBody {
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
