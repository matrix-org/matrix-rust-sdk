use crate::Room;
use language_tags::LanguageTag;
use url::Url;
use urlencoding::encode;
/// Information about a widget.
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
    /// * `parent_url` - The parent url is used as the target for the
    ///   postMessages send by the widget
    /// Should be the url of the app hosting the widget.
    /// In case the app hosting the widget is not a webapp the platform specific
    /// value needs to be used or `"*"` a wildcard.
    /// Be aware that this means the widget will receive its own postmessage
    /// messages. The (js) matrix-widget-api ignores those however so this
    /// works but it might break custom implementations. So always keep this
    /// in mind.
    /// * `font_scale` - The font scale used in the widget.
    /// This should be in sync with the current client app configuration
    /// * `lang` - the language e.g. en-us
    /// * `analytics_id` - This can be used in case the widget wants to connect
    ///   to the
    /// same analytics provider as the client app only set this value on widgets
    /// which are known.
    pub fn get_url(
        &self,
        room: Room,
        parent_url: &str,
        font_scale: f64,
        lang: LanguageTag,
        room_id: &str,
    ) -> String {
        // All arguement that are also used by other widgets will be replaced in this step
        self.raw_url
            .as_str()
            .replace("$widgetId", &self.id)
            .replace("$parentUrl", &encode(parent_url))
            .replace("$userId", room.own_user_id().as_str())
            .replace("$lang", lang.as_str())
            .replace("$fontScale", &font_scale.to_string())
            .replace("$roomId", &encode(room_id))
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
    /// * `hide_header` for Element Call this defines if the branding header should be hidden.
    /// * `preload` if set, the lobby will be skipped and the widget will join the call on the `io.element.join` action.
    /// * `base_url` the url of the matrix homserver in use e.g. https://matrix-client.matrix.org.
    /// * `analytics_id` can be used to pass a posthog id.
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
