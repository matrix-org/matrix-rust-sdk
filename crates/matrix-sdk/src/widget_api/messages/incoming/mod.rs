pub mod messages;
pub mod reply;

pub use self::{
    messages::{Incoming, SupportedVersions, ApiVersion},
    reply::Reply,
};
pub use super::super::Error;

pub struct Request<C, R> {
    pub content: C,
    reply: Reply<C, R>,
}

impl <C, R> Request<C, R> {
    pub fn reply(self, response: R) -> Result<(), Error> {
        self.reply.reply(response).map_err(|_| Error::WidgetDied)
    }
}

