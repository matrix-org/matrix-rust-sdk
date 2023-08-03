use crate::widget_api::messages;
use super::handler::Result;



// the Widget trait that needs to be implemented in the native client
pub trait Widget {
    /// This is the most important message to be implemented in the client driver
    /// the function will be called on the struct implementing the client driver whenever there is a new message available
    /// that has to be sent to the widget
    fn send_widget_message(&self, message: &str) -> Result<()>;

    fn show_capability_request(&self, cap: messages::capabilities::Options);
    fn id(&self) -> &str;
    fn get_widget_state_json(&self) -> &str;
}
pub struct DummyWidget {}
impl Widget for DummyWidget {
    fn send_widget_message(&self, message: &str) -> Result<()> {
        todo!()
    }

    fn show_capability_request(&self, cap: messages::capabilities::Options) {
        todo!()
    }

    fn id(&self) -> &str {
        todo!()
    }

    fn get_widget_state_json(&self) -> &str {
        todo!()
    }
}
