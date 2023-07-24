pub mod message_types;
pub mod reply;

pub use self::{
    message_types::{ApiVersion, FromWidgetMessage, SupportedVersions},
    reply::Reply,
};
pub use super::super::Error;

pub struct Request<ReqBody, ResBody> {
    pub content: ReqBody,
    reply: Reply<ReqBody, ResBody>,
}

impl<C, R> Request<C, R> {
    pub fn reply(self, response: R) -> Result<(), Error> {
        self.reply.reply(response).map_err(|_| Error::WidgetDied)
    }
}
