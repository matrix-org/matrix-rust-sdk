use async_trait::async_trait;

use crate::widget_api::{error::Result, messages::capabilities::Options as Capabilities};

/// The Widget trait that needs to be implemented in the native client, i.e. by an actual
/// "widget handler" that is going to abstract away the widget-specific functionality.
#[async_trait]
pub trait Widget: Send + Sync + 'static {
    /// This function will be called whenever there is a new message available
    /// that has to be sent to the widget.
    ///
    /// # Examples
    /// ```
    /// implement Widget for MyWidget {
    ///     fn send(&self, json: &str) -> Result<()> {
    ///         myWidgetIFrame.postmessage(message);
    ///         Ok()
    ///     }
    /// }
    ///```
    fn send(&self, json: &str) -> Result<()>;

    /// The client should show a dialog to give the user the option approve some/all of the capabilites that the widget requests.
    /// The returned value contains the capability options the user has approved. If the client interracts with a user, showing the
    /// permission request, the client should also provide good phrasing for the different permissions/filters.
    async fn acquire_permissions(&self, cap: Capabilities) -> Result<Capabilities>;

    /// Returns the widget id from the widget state event.
    fn id(&self) -> &str;
}
