use backoff::default;
use language_tags::LanguageTag;
use ruma::api::client::profile::get_profile;
use url::Url;
use urlencoding::encode;

use crate::Room;

mod url_props {
    pub static WIDGET_ID: &str = "$matrix_widget_id";
    pub static AVATAR_URL: &str = "$matrix_avatar_url";
    pub static DISPLAY_NAME: &str = "$matrix_display_name";
    pub static USER_ID: &str = "$matrix_user_id";
    pub static ROOM_ID: &str = "$matrix_room_id";
    pub static LANGUAGE: &str = "$org.matrix.msc2873.client_language";
    pub static CLIENT_THEME: &str = "$org.matrix.msc2873.client_theme";
    pub static CLIENT_ID: &str = "$org.matrix.msc2873.client_id";
    pub static DEVICE_ID: &str = "$org.matrix.msc2873.matrix_device_id";
    pub static BASE_URL: &str = "$org.matrix.msc4039.matrix_base_url";
}
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
    /// to the widget. Possible values are: `$matrix_widget_id`, `$parentUrl`...
    ///
    /// # Examples
    ///
    /// e.g `http://widget.domain?username=$userId`
    /// will become: `http://widget.domain?username=@user_matrix_id:server.domain`.
    raw_url: String,
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

    pub async fn generate_url(
        &self,
        room: &Room,
        props: ClientProperties,
    ) -> Result<Url, url::ParseError> {
        let profile = room
            .client()
            .account()
            .get_profile()
            .await
            .unwrap_or(get_profile::v3::Response::new(None, None));
        let avatar_url = profile.avatar_url.map(|url| url.to_string()).unwrap_or("".to_owned());
        let device_id = room.client().device_id().map(|d| d.to_string()).unwrap_or("".to_owned());

        Url::parse(
            &self
                .raw_url
                .as_str()
                .replace(url_props::WIDGET_ID, &self.id)
                .replace(url_props::AVATAR_URL, &avatar_url)
                .replace(url_props::DEVICE_ID, &device_id)
                .replace(url_props::DISPLAY_NAME, &profile.displayname.unwrap_or("".to_owned()))
                .replace(url_props::BASE_URL, &encode(&room.client().homeserver().await.as_str()))
                .replace(url_props::USER_ID, room.own_user_id().as_str())
                .replace(url_props::ROOM_ID, &encode(room.room_id().as_str()))
                .replace(url_props::LANGUAGE, props.language.as_str())
                .replace(url_props::CLIENT_THEME, props.theme.as_str())
                .replace(url_props::CLIENT_ID, &props.client_id),
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
    /// * `element_call_path` - the path to the app e.g. https://call.element.io,
    ///   https://call.element.dev
    /// * `id` - the widget id.
    /// * `embed` - the embed param for the widget.
    /// * `hide_header` - for Element Call this defines if the branding header
    ///   should be hidden.
    /// * `preload` - if set, the lobby will be skipped and the widget will join
    ///   the call on the `io.element.join` action.
    /// * `font_scale` - The font scale which will be used inside element call.
    /// * `analytics_id` - Can be used to pass a posthog id to element call.
    /// * `parent_url` The url that is used as the target for the PostMessages
    ///   sent by the widget (to the client). For a web app client this is the
    ///   client url. In case of using other platforms the client most likely is
    ///   setup up to listen to postmessages in the same webview the widget is
    ///   hosted. In this case the parent_url is set to the url of the webview
    ///   with the widget. Be aware, that this means, the widget will receive
    ///   its own postmessage messages. The matrix-widget-api (js) ignores those
    ///   so this works but it might break custom implementations. So always
    ///   keep this in mind.
    pub fn new_virtual_element_call_widget(
        element_call_path: &str,
        id: String,
        embed: bool,
        hide_header: bool,
        preload: bool,
        font_scale: f64,
        analytics_id: Option<&str>,
        parent_url: &str,
    ) -> Self {
        let mut raw_url = format!("{element_call_path}?");

        // Default widget url template parameters:
        raw_url.push_str(&format!("?widgetId={}", url_props::WIDGET_ID));
        raw_url.push_str(&format!("&userId={}", url_props::USER_ID));
        raw_url.push_str(&format!("&deviceId={}", url_props::DEVICE_ID));
        raw_url.push_str(&format!("&roomId={}", url_props::WIDGET_ID));
        raw_url.push_str(&format!("&lang={}", url_props::LANGUAGE));
        raw_url.push_str(&format!("&theme={}", url_props::CLIENT_THEME));
        raw_url.push_str(&format!("&baseUrl={}", url_props::BASE_URL));

        // Custom element call url parameters:
        raw_url.push_str(&format!("&parentUrl={}", encode(&parent_url)));
        if embed {
            raw_url.push_str("&embed=")
        }
        if hide_header {
            raw_url.push_str("&hideHeader=")
        }
        if preload {
            raw_url.push_str("&preload=")
        }
        if let Some(analytics_id) = analytics_id {
            raw_url.push_str(&format!("&analyticsID={}", encode(analytics_id)));
        }
        raw_url.push_str(&format!("&fontScale={}", &font_scale.to_string()));

        // for EC we always want init on load to be false
        Self { id, init_on_load: false, raw_url }
    }

    /// Create a new WidgetSettings instance
    pub fn new(id: String, init_on_load: bool, raw_url: String) -> Self {
        Self { id, init_on_load, raw_url }
    }
    // TODO: add From<WidgetStateEvent> so that WidgetSetting can be build
    // by using the room state directly:
    // Something like: room.get_widgets() -> Vec<WidgetStateEvent>
}

/// The set of settings and propterties for the widget based on the client
/// configuration. Those values are used generate the widget url.
#[derive(Debug)]
pub struct ClientProperties {
    /// The language the client is set to e.g. en-us.
    pub language: LanguageTag,
    /// The client_id provides the widget with the option to behave differently
    /// for different clients. e.g org.example.ios.
    pub client_id: String,
    /// A string describing the theme (dark, light) or org.example.dark.
    pub theme: String,
}
impl ClientProperties {
    /// Create client Properties with a String as the language_tag.
    /// If a malformatted language_tag is provided it will default to en-US.
    pub fn new(language: String, client_id: String, theme: String) -> Self {
        // its save to unwrap "en-us"
        let default = LanguageTag::parse("en-us").unwrap();
        ClientProperties {
            language: LanguageTag::parse(&language).unwrap_or(default),
            client_id,
            theme,
        }
    }
}
