use language_tags::LanguageTag;
use url::Url;
use urlencoding::encode;

use crate::Room;

/// Settings of the widget.
#[derive(Debug)]
pub struct WidgetSettings {
    /// Widget's unique identifier.
    pub id: String,

    /// Whether or not the widget should be initialized on load message
    /// (`ContentLoad` message), or upon creation/attaching of the widget to
    /// the SDK's state machine that drives the API.
    pub init_on_load: bool,

    /// This contains the url from the widget state event.
    /// In this url placeholders can be used to pass information from the client
    /// to the widget. Possible values are: `$widgetId`, `$parentUrl`,
    /// `$userId`, `$lang`, `$fontScale`, `$analyticsID`.
    ///
    /// # Examples
    ///
    /// e.g `http://widget.domain?username=$userId`
    /// will become: `http://widget.domain?username=@user_matrix_id:server.domain`.
    raw_url: Url,
}

impl WidgetSettings {
    /// Create the actual Url that can be used to setup the WebView or IFrame
    /// that contains the widget.
    ///
    /// # Arguments
    ///
    /// * `room` - A matrix room which is used to query the logged in username
    /// * `props` - Propertis from the client that can be used by a widget to
    ///   adapt to the client. e.g. language, font-scale...

    pub fn generate_url(
        &self,
        room: &Room,
        props: ClientProperties,
    ) -> Result<Url, url::ParseError> {
        Url::parse(
            &self
                .raw_url
                .as_str()
                .replace("$widgetId", &self.id)
                .replace("$parentUrl", &encode(&props.parent_url))
                .replace("$userId", room.own_user_id().as_str())
                .replace("$lang", props.lang.as_str())
                .replace("$fontScale", &props.font_scale.to_string())
                .replace("$roomId", &encode(room.room_id().as_str())),
        )
    }

    /// `WidgetSettings` are usually created from a state event.
    /// (currently unimplemented)
    /// But in some cases the client wants to create custom `WidgetSettings`
    /// for specific rooms based on other conditions.
    /// This function returns a `WidgetSettings` object which can be used
    /// to setup a widget using `run_client_widget_api`
    /// and to generate the correct url for the widget.
    ///
    /// # Arguments
    /// * `base_path` the path to the app e.g. https://call.element.io.
    /// * `id` the widget id.
    /// * `embed` the embed param for the widget.
    /// * `hide_header` for Element Call this defines if the branding header
    ///   should be hidden.
    /// * `preload` if set, the lobby will be skipped and the widget will join
    ///   the call on the `io.element.join` action.
    /// * `base_url` the url of the matrix homserver in use e.g. https://matrix-client.matrix.org.
    /// * `analytics_id` can be used to pass a posthog id to element call.
    pub fn new_virtual_element_call_widget(
        base_path: &str,
        id: String,
        embed: bool,
        hide_header: bool,
        device_id: &str,
        preload: bool,
        base_url: &str,
        analytics_id: Option<&str>,
    ) -> Result<Self, url::ParseError> {
        let mut raw_url = format!("{base_path}?");

        raw_url.push_str(&format!("widgetId=$widgetId"));
        raw_url.push_str("&parentUrl=$parentUrl");
        if embed {
            raw_url.push_str("&embed=")
        }
        if hide_header {
            raw_url.push_str("&hideHeader=")
        }
        raw_url.push_str("&userId=$userId");
        raw_url.push_str(&format!("&deviceId={device_id}"));
        raw_url.push_str("&roomId=$roomId");
        raw_url.push_str("&lang=$lang");
        raw_url.push_str("&fontScale=$fontScale");
        if preload {
            raw_url.push_str("&preload=")
        }
        raw_url.push_str(&format!("&baseUrl={}", encode(base_url)));
        if let Some(analytics_id) = analytics_id {
            raw_url.push_str(&format!("&analyticsID={}", encode(analytics_id)));
        }
        let raw_url = Url::parse(&raw_url)?;
        // for EC we always want init on laod to be false
        Ok(Self { id, init_on_load: false, raw_url })
    }

    // TODO: add From<WidgetStateEvent> so that WidgetSetting can be build
    // by using the room state directly:
    // Something like: room.get_widgets() -> Vec<WidgetStateEvent>
}

/// The set of settings and propterties for the widget based on the client
/// configuration. Those values are used generate the widget url.
#[derive(Debug)]
pub struct ClientProperties {
    /// The url that is used as the target for the PostMessages sent by the
    /// widget (to the client). For a web app client this is the client url.
    /// In case of using other platforms the client most likely is
    /// setup up to listen to postmessages in the same webview the widget is
    /// hosted. In this case the parent_url is set to the url of the webview
    /// with the widget.
    /// Be aware, that this means, the widget will receive its own postmessage
    /// messages. The matrix-widget-api (js) ignores those so this
    /// works but it might break custom implementations. So always keep this
    /// in mind.
    parent_url: String,
    /// The font scale configured in the client.
    font_scale: f64,
    /// The language the client is set to e.g. en-us.
    lang: LanguageTag,
}
