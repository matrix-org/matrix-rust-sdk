use async_trait::async_trait;

use super::handler::Result;
use crate::widget_api::messages::capabilities;

/// The Widget trait that needs to be implemented in the native client.
/// An instance implementing this trait will be required to initialize the widget driver.
#[async_trait]
pub trait Widget {
    /// This function will be called whenever there is a new message available
    /// that has to be sent to the widget.
    ///
    /// # Examples
    /// ```
    /// implement Widget for MyWidget {
    ///     fn send_widget_message(&self, message: &str) -> Result<()> {
    ///         myWidgetIFrame.postmessage(message);
    ///         Ok()
    ///     }
    /// }
    ///```
    fn send_widget_message(&self, message: &str) -> Result<()>;

    /// The client should show a dialog to give the user the option approve some/all of the capabilites that the widget requests.
    /// The returned value contains the capability options the user has approved.
    /// The client should also provide good phrasing for the different permissions/filters:
    /// cap.send_room_event = [{event_type: "m.room.message", msgtype: "m.image" }] should be come sth like:
    /// Allow the widget to, send images in this room. (checkbox)
    async fn capability_permissions(
        &self,
        cap: capabilities::Options,
    ) -> Result<capabilities::Options>;

    /// The client should show a dialog to approve if the widget is allowed to get an OpenId token.
    /// A `OpenIdDialogResponse` is returned containing a flag if the user alloed the token request and
    /// if the permission can be cached so the widget driver will not call `openid_permissions` on the widget again.
    async fn openid_permissions(&self) -> Result<OpenIdDialogResponse>;

    /// Returns the widget id from the widget state event.
    fn id(&self) -> &str;

    /// Returns the widget state event as a raw string.
    fn get_widget_state_json(&self) -> &str;
}

#[derive(Debug)]
pub struct OpenIdDialogResponse {
    is_allowed: bool,
    cache_permission: bool,
}

#[derive(Debug)]
pub struct DummyWidget {}

#[async_trait]
impl Widget for DummyWidget {
    fn send_widget_message(&self, _message: &str) -> Result<()> {
        todo!()
    }

    async fn capability_permissions(
        &self,
        cap: capabilities::Options,
    ) -> Result<capabilities::Options> {
        Ok(cap)
    }
    async fn openid_permissions(&self) -> Result<OpenIdDialogResponse> {
        Ok(OpenIdDialogResponse { is_allowed: true, cache_permission: true })
    }

    fn id(&self) -> &str {
        todo!()
    }

    fn get_widget_state_json(&self) -> &str {
        todo!()
    }
}
