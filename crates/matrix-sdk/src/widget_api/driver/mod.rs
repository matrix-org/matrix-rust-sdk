use async_trait::async_trait;

use super::capabilities::Capabilities;
use super::handler::{self, OpenIDState};
use super::handler::{Outgoing, Result};
use super::messages::capabilities::Options;
use super::messages::openid;
use super::Error;
use crate::room::Joined;

pub mod widget;
use widget::Widget;

#[derive(Debug)]
pub struct Driver<W: Widget> {
    pub matrix_room: Joined,
    pub widget: W,
}
#[async_trait(?Send)]
impl<W: Widget> handler::Driver for Driver<W> {
    fn initialise(&self, options: Options) -> Result<Capabilities> {
        unimplemented!()
    }
    async fn send(&self, message: Outgoing) -> Result<()> {
        unimplemented!()
    }

    async fn get_openid(&self, req: openid::Request) -> OpenIDState {
        let userId = self.matrix_room.client.user_id().unwrap();
        let request =
            ruma::api::client::account::request_openid_token::v3::Request::new(userId.to_owned());
        let res = self.matrix_room.client.send(request, None).await;

        if let Err(err) = res {
            return OpenIDState::Resolved(Err(Error::WidgetError(
                format!(
                    "Failed to get an open id token from the homeserver. Because of Http Error: {}",
                    err.to_string()
                )
                .to_owned(),
            )));
        };

        let res = res.unwrap();
        let openid_response = openid::Response {
            id: req.id,
            token: res.access_token,
            expires_in_seconds: res.expires_in.as_secs() as usize,
            server: res.matrix_server_name.to_string(),
            kind: res.token_type.to_string(),
        };
        OpenIDState::Resolved(Ok(openid_response))
    }
}
