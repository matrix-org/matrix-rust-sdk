//! Widget API implementation.

use async_channel::{Receiver, Sender};
use tokio::sync::mpsc::unbounded_channel;
use url::Url;
use urlencoding::encode;

use crate::room::Room as JoinedRoom;

mod client;
mod filter;
mod permissions;

use self::client::{Action, ClientApi, Event};
pub use self::{
    filter::{EventFilter, MessageLikeEventFilter, StateEventFilter},
    permissions::{Permissions, PermissionsProvider},
};

/// Describes a widget.
#[derive(Debug)]
pub struct Widget {
    /// Settings for the widget.
    pub settings: WidgetSettings,
    /// Communication channels with a widget.
    pub comm: Comm,
}

/// Communication "pipes" with a widget.
#[derive(Debug)]
pub struct Comm {
    /// Raw incoming messages from the widget (normally, formatted as JSON).
    ///
    /// These can be both requests and responses. Users of this API should not
    /// care what's what though because they are only supposed to forward
    /// messages between the webview / iframe, and the SDK's widget driver.
    pub from: Receiver<String>,
    /// Raw outgoing messages from the client (SDK) to the widget (normally
    /// formatted as JSON).
    ///
    /// These can be both requests and responses. Users of this API should not
    /// care what's what though because they are only supposed to forward
    /// messages between the webview / iframe, and the SDK's widget driver.
    pub to: Sender<String>,
}

/// Information about a widget.
#[derive(Debug)]
pub struct WidgetSettings {
    /// Widget's unique identifier.
    pub id: String,
    /// Whether or not the widget should be initialized on load message
    /// (`ContentLoad` message), or upon creation/attaching of the widget to
    /// the SDK's state machine that drives the API.
    pub init_on_load: bool,
    /// This contains the url from the widget state event
    /// In this url placeholders can be used to pass information from the client to the widget
    /// Possible values are: `$widgetId`, `$parentUrl`, `$userId`, `$lang`, `$fontScale`, `$analyticsID`
    /// 
    /// # Examples
    /// 
    /// e.g `http://widget.domain?username=$userId`
    /// will become: `http://widget.domain?username=@user_matrix_id:server.domain`
    raw_url: Url,
}
impl WidgetSettings {
    /// Create the actual Url that can be used to setup the WebView or IFrame that contains the widget.
    /// 
    /// # Arguments
    /// 
    /// * `room` - A matrix room which is used to query the logged in username
    /// * `parent_url` - The parent url is used as the target for the postMessages send by the widget
    /// Should be the url of the app hosting the widget.
    /// In case the app hosting the widget is not a webapp the platform specific 
    /// value needs to be used or `"*"` a wildcard.
    /// Be aware that this means the widget will receive its own postmessage messages.
    /// The (js) matrix-widget-api ignores those however so this works but it might break
    /// custom implementations. So always keep this in mind.
    /// * `font_scale` - The font scale used in the widget. 
    /// This should be in sync with the current client app configuration
    /// * `lang` - the language e.g. en-us
    /// * `analytics_id` - This can be used in case the widget wants to connect to the 
    /// same analytics provider as the client app only set this value on widgets which are known.
    pub fn get_url(
        &self,
        room: JoinedRoom,        
        parent_url: String,
        font_scale: f64,
        lang: String,
        analytics_id: String,
    ) -> String {
        self.raw_url
            .as_str()
            .replace("$widgetId", &self.id)
            .replace("$parentUrl", &encode(parent_url.as_str()))
            .replace("$userId", &room.client().user_id().unwrap().to_string())
            .replace("$lang", &lang)
            .replace("$fontScale", &font_scale.to_string())
            .replace("$analyticsID", &analytics_id)
    }

    /// WidgetSettings are usually created from a state event.
    /// (currently unimplemented)
    /// But in some cases the client wants to create custom `WidgetSettings`
    /// for specific rooms based on other conditions.
    /// This function returns a WidgetSettings object which can be used
    /// to setup a widget using `run_client_widget_api`
    /// and to generate the correct url for the widget.
    pub fn new_virtual_widget(
        id: String,
        init_on_load: bool,
        base_path: &str,
    ) -> Result<Self, url::ParseError> {
        let raw_url = 
            format!("{base_path}?widgetId=$widgetId&parentUrl=$parentUrl#?&userId=$userId&lang=$lang&fontScale=$fontScale&analyticsID=$analyticsID", base_path= base_path);
        let raw_url = Url::parse(&raw_url)?;
        Ok(Self { id, init_on_load, raw_url })
    }

    // TODO: add From<WidgetStateEvent> so that WidgetSetting can be build
    // by using the room state directly:
    // Something like: room.get_widgets() -> Vec<WidgetStateEvent>
}

/// Starts a client widget API state machine for a given `widget` in a given
/// joined `room`. The function returns once the widget is disconnected or any
/// terminal error occurs.
///
/// Not implemented yet! Currently, it does not contain any useful
/// functionality, it only blindly forwards the messages and returns errors once
/// a non-implemented part is triggered.
pub async fn run_widget_api(
    _room: JoinedRoom,
    widget: Widget,
    _permissions_provider: impl PermissionsProvider,
) -> Result<(), ()> {
    let Comm { from, to } = widget.comm;

    // Create a channel so that we can conveniently send all events to it.
    let (events_tx, mut events_rx) = unbounded_channel();

    // Forward all incoming raw messages into events and send them to the sink.
    // Equivalent of the:
    // `from.map(|m| Ok(Event::MessageFromWidget(msg)).forward(events_tx)`,
    // but apparently `UnboundedSender<T>` does not implement `Sink<T>`.
    let tx = events_tx.clone();
    tokio::spawn(async move {
        while let Ok(msg) = from.recv().await {
            let _ = tx.send(Event::MessageFromWidget(msg));
        }
    });

    // Process events by passing them to the `ClientApi` implementation.
    let mut client_api = ClientApi::new();
    while let Some(event) = events_rx.recv().await {
        for action in client_api.process(event) {
            match action {
                Action::SendToWidget(msg) => to.send(msg).await.map_err(|_| ())?,
                Action::AcquirePermissions(cmd) => {
                    let result = cmd.result(Err("not implemented".into()));
                    events_tx.send(Event::PermissionsAcquired(result)).map_err(|_| ())?;
                }
                Action::GetOpenId(cmd) => {
                    let result = cmd.result(Err("not implemented".into()));
                    events_tx.send(Event::OpenIdReceived(result)).map_err(|_| ())?;
                }
                Action::ReadMatrixEvent(cmd) => {
                    let result = cmd.result(Err("not implemented".into()));
                    events_tx.send(Event::MatrixEventRead(result)).map_err(|_| ())?;
                }
                Action::SendMatrixEvent(cmd) => {
                    let result = cmd.result(Err("not implemented".into()));
                    events_tx.send(Event::MatrixEventSent(result)).map_err(|_| ())?;
                }
                Action::Subscribe => {}
                Action::Unsubscribe => {}
            }
        }
    }

    Ok(())
}
