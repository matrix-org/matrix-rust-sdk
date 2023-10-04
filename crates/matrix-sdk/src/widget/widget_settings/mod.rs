pub use element_call::VirtualElementCallWidgetOptions;
use language_tags::LanguageTag;
use ruma::{api::client::profile::get_profile, DeviceId, RoomId, UserId};
use url::Url;

use crate::Room;

mod element_call;

mod url_params {
    use url::Url;
    use urlencoding::encode;

    pub static USER_ID: &str = "$matrix_user_id";
    pub static ROOM_ID: &str = "$matrix_room_id";
    pub static WIDGET_ID: &str = "$matrix_widget_id";
    pub static AVATAR_URL: &str = "$matrix_avatar_url";
    pub static DISPLAY_NAME: &str = "$matrix_display_name";
    pub static LANGUAGE: &str = "$org.matrix.msc2873.client_language";
    pub static CLIENT_THEME: &str = "$org.matrix.msc2873.client_theme";
    pub static CLIENT_ID: &str = "$org.matrix.msc2873.client_id";
    pub static DEVICE_ID: &str = "$org.matrix.msc2873.matrix_device_id";
    pub static HOMESERVER_URL: &str = "$org.matrix.msc4039.matrix_base_url";

    pub struct QueryProperties {
        pub(crate) widget_id: String,
        pub(crate) avatar_url: String,
        pub(crate) display_name: String,
        pub(crate) user_id: String,
        pub(crate) room_id: String,
        pub(crate) language: String,
        pub(crate) client_theme: String,
        pub(crate) client_id: String,
        pub(crate) device_id: String,
        pub(crate) homeserver_url: String,
    }
    pub fn replace_properties(url: &mut Url, props: QueryProperties) {
        let replace_map: [(&str, String); 10] = [
            (WIDGET_ID, encode(&props.widget_id).into()),
            (AVATAR_URL, encode(&props.avatar_url).into()),
            (DEVICE_ID, encode(&props.device_id).into()),
            (DISPLAY_NAME, encode(&props.display_name).into()),
            (HOMESERVER_URL, encode(&props.homeserver_url).into()),
            (USER_ID, encode(&props.user_id).into()),
            (ROOM_ID, encode(&props.room_id).into()),
            (LANGUAGE, encode(&props.language).into()),
            (CLIENT_THEME, encode(&props.client_theme).into()),
            (CLIENT_ID, encode(&props.client_id).into()),
        ]
        .map(|to_replace| {
            // Its save to unwrap here since we know all replace strings start with `$`
            (to_replace.0.get(1..).unwrap(), to_replace.1)
        });

        let s = url.as_str();
        let Some(beginning) = s.split_once("$").map(|s| s.0) else {
            // There is no $ in the string so we don't need to do anything
            return;
        };
        let mut result = String::from(beginning);
        for section in s.split("$").skip(1) {
            let mut section_added = false;
            for (old, new) in &replace_map {
                // save to unwrap here since we know all replace strings start with $
                if section.starts_with(old) {
                    result.push_str(new);
                    if let Some(rest) = section.get(old.len()..) {
                        result.push_str(rest);
                    }
                    section_added = true;
                }
            }
            if !section_added {
                result.push_str(section);
            }
        }
        *url = Url::parse(&result).unwrap();
    }
}

/// Settings of the widget.
#[derive(Debug, Clone)]
pub struct WidgetSettings {
    id: String,

    init_after_content_load: bool,

    raw_url: Url,
}

impl WidgetSettings {
    /// Widget's unique identifier.
    pub fn id(&self) -> &String {
        &self.id
    }

    /// Whether or not the widget should be initialized on load message
    /// (`ContentLoad` message), or upon creation/attaching of the widget to
    /// the SDK's state machine that drives the API.

    pub fn init_after_content_load(&self) -> bool {
        self.init_after_content_load
    }

    /// This contains the url from the widget state event.
    /// In this url placeholders can be used to pass information from the client
    /// to the widget. Possible values are: `$matrix_widget_id`,
    /// `$matrix_display_name`...
    ///
    /// # Examples
    ///
    /// e.g `http://widget.domain?username=$userId`
    /// will become: `http://widget.domain?username=@user_matrix_id:server.domain`.
    pub fn raw_url(&self) -> &Url {
        &self.raw_url
    }

    /// Get the base url of the widget. Used as the target for PostMessages. In
    /// case the widget is in a webview and not an IFrame. It contains the schema and the authority e.g. `https://my.domain.org`
    /// A postmessage would be send using: `postmessage(myMessage,
    /// widget_base_url)`
    pub fn base_url(&self) -> Option<Url> {
        base_url(&self.raw_url)
    }

    /// Create the actual [`Url`] that can be used to setup the WebView or
    /// IFrame that contains the widget.
    ///
    /// # Arguments
    ///
    /// * `room` - A matrix room which is used to query the logged in username
    /// * `props` - Properties from the client that can be used by a widget to
    ///   adapt to the client. e.g. language, font-scale...
    pub async fn generate_webview_url(
        &self,
        room: &Room,
        props: ClientProperties,
    ) -> Result<Url, url::ParseError> {
        self._generate_webview_url(
            room.client().account().get_profile().await.unwrap_or_default(),
            room.own_user_id(),
            room.room_id(),
            room.client().device_id().unwrap_or("UNKNOWN".into()),
            room.client().homeserver(),
            props,
        )
    }

    // Using a separate function (without Room as a param) for tests.
    fn _generate_webview_url(
        &self,
        profile: get_profile::v3::Response,
        user_id: &UserId,
        room_id: &RoomId,
        device_id: &DeviceId,
        homeserver_url: Url,
        client_props: ClientProperties,
    ) -> Result<Url, url::ParseError> {
        let avatar_url = profile.avatar_url.map(|url| url.to_string()).unwrap_or_default();

        let query_props = url_params::QueryProperties {
            widget_id: self.id.clone(),
            avatar_url,
            display_name: profile.displayname.unwrap_or_default(),
            user_id: user_id.into(),
            room_id: room_id.into(),
            language: client_props.language.to_string(),
            client_theme: client_props.theme,
            client_id: client_props.client_id,
            device_id: device_id.into(),
            homeserver_url: homeserver_url.into(),
        };
        let mut generated_url = self.raw_url.clone();
        url_params::replace_properties(&mut generated_url, query_props);

        Ok(generated_url)
    }
    /// Create a new WidgetSettings instance
    pub fn new(
        id: String,
        init_after_content_load: bool,
        raw_url: &str,
    ) -> Result<Self, url::ParseError> {
        Ok(Self { id, init_after_content_load, raw_url: Url::parse(raw_url)? })
    }
    // TODO: add From<WidgetStateEvent> so that WidgetSetting can be build
    // by using the room state directly:
    // Something like: room.get_widgets() -> Vec<WidgetStateEvent>
}

/// The set of settings and properties for the widget based on the client
/// configuration. Those values are used generate the widget url.
#[derive(Debug)]
pub struct ClientProperties {
    /// The client_id provides the widget with the option to behave differently
    /// for different clients. e.g org.example.ios.
    client_id: String,
    /// The language the client is set to e.g. en-us.
    language: LanguageTag,
    /// A string describing the theme (dark, light) or org.example.dark.
    theme: String,
}
impl ClientProperties {
    /// Create client Properties with a String as the language_tag.
    /// If a malformatted language_tag is provided it will default to en-US.
    /// # Arguments
    /// * `client_id` the client identifier. This allows widgets to adapt to
    ///   specific clients (e.g. `io.element.web`)
    /// * `language` the language that is used in the client. (default: `en-US`)
    /// * `theme` the theme (dark, light) or org.example.dark. (default:
    ///   `light`)
    pub fn new(client_id: &str, language: Option<LanguageTag>, theme: Option<String>) -> Self {
        // We use a String for the language here, so we don't need to import LanguageTag
        // for the bindings crate. The conversion is done here.
        // its save to unwrap "en-us"
        let default_language = LanguageTag::parse("en-us").unwrap();
        let default_theme = "light".to_owned();
        Self {
            language: language.unwrap_or(default_language),
            client_id: client_id.to_owned(),
            theme: theme.unwrap_or(default_theme),
        }
    }
}

fn base_url(url: &Url) -> Option<Url> {
    let mut url = url.clone();
    match url.path_segments_mut() {
        Ok(mut path) => path.clear(),
        Err(_) => return None,
    };
    url.set_query(None);
    url.set_fragment(None);
    Some(url)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use ruma::api::client::profile::get_profile;
    use url::Url;

    use super::{
        element_call::VirtualElementCallWidgetOptions,
        url_params::{replace_properties, QueryProperties},
        WidgetSettings,
    };
    use crate::widget::ClientProperties;

    const EXAMPLE_URL: &str = "\
    https://my.widget.org/custom/path\
    ?widgetId=$matrix_widget_id\
    &deviceId=$org.matrix.msc2873.matrix_device_id\
    &avatarUrl=$matrix_avatar_url\
    &displayname=$matrix_display_name\
    &lang=$org.matrix.msc2873.client_language\
    &theme=$org.matrix.msc2873.client_theme\
    &clientId=$org.matrix.msc2873.client_id\
    &baseUrl=$org.matrix.msc4039.matrix_base_url\
    ";

    const WIDGET_ID: &str = "1/@#w23";

    fn get_example_url() -> Url {
        Url::parse(EXAMPLE_URL).expect("EXAMPLE_URL is malformatted")
    }

    fn get_example_props() -> QueryProperties {
        QueryProperties {
            widget_id: String::from("!@/abc_widget_id"),
            avatar_url: "!@/abc_avatar_url".to_owned(),
            display_name: "!@/abc_display_name".to_owned(),
            user_id: "!@/abc_user_id".to_owned(),
            room_id: "!@/abc_room_id".to_owned(),
            language: "!@/abc_language".to_owned(),
            client_theme: "!@/abc_client_theme".to_owned(),
            client_id: "!@/abc_client_id".to_owned(),
            device_id: "!@/abc_device_id".to_owned(),
            homeserver_url: "!@/abc_base_url".to_owned(),
        }
    }

    fn get_widget_settings() -> WidgetSettings {
        WidgetSettings::new_virtual_element_call_widget(VirtualElementCallWidgetOptions {
            element_call_url: "https://call.element.io".to_owned(),
            widget_id: WIDGET_ID.to_owned(),
            parent_url: None,
            hide_header: Some(true),
            preload: Some(true),
            font_scale: None,
            app_prompt: Some(true),
            skip_lobby: Some(false),
            confine_to_room: Some(true),
            fonts: None,
            analytics_id: None,
        })
        .expect("could not parse virtual element call widget")
    }

    // Convert query strings to HashSet so that we can compare the urls independent
    // of the order of the params.
    trait FragmentQuery {
        fn fragment_query(&self) -> Option<&str>;
    }

    impl FragmentQuery for Url {
        fn fragment_query(&self) -> Option<&str> {
            Some(self.fragment()?.split_once('?')?.1)
        }
    }

    type QuerySet = HashSet<(String, String)>;

    use serde_html_form::from_str;

    fn get_query_sets(url: &Url) -> Option<(QuerySet, QuerySet)> {
        let fq = from_str::<QuerySet>(url.fragment_query().unwrap_or_default()).ok()?;
        let q = from_str::<QuerySet>(url.query().unwrap_or_default()).ok()?;
        Some((q, fq))
    }

    #[test]
    fn replace_all_properties() {
        let mut url = get_example_url();

        const CONVERTED_URL: &str = "https://my.widget.org/custom/path?widgetId=%21%40%2Fabc_widget_id&deviceId=%21%40%2Fabc_device_id&avatarUrl=%21%40%2Fabc_avatar_url&displayname=%21%40%2Fabc_display_name&lang=%21%40%2Fabc_language&theme=%21%40%2Fabc_client_theme&clientId=%21%40%2Fabc_client_id&baseUrl=%21%40%2Fabc_base_url";

        replace_properties(&mut url, get_example_props());
        assert_eq!(url.as_str(), CONVERTED_URL);
    }

    #[test]
    fn new_virtual_element_call_widget_base_url() {
        let widget_settings = get_widget_settings();
        assert_eq!(widget_settings.base_url().unwrap().as_str(), "https://call.element.io/");
    }

    #[test]
    fn new_virtual_element_call_widget_raw_url() {
        const CONVERTED_URL:&str= "https://call.element.io/room#?userId=$matrix_user_id&roomId=$matrix_room_id&widgetId=$matrix_widget_id&avatarUrl=$matrix_avatar_url&displayname=$matrix_display_name&lang=$org.matrix.msc2873.client_language&theme=$org.matrix.msc2873.client_theme&clientId=$org.matrix.msc2873.client_id&deviceId=$org.matrix.msc2873.matrix_device_id&baseUrl=$org.matrix.msc4039.matrix_base_url&parentUrl=https%3A%2F%2Fcall.element.io&skipLobby=false&confineToRoom=true&appPrompt=true&hideHeader=true&preload=true";

        let mut url = get_widget_settings().raw_url().clone();
        let mut gen = Url::parse(CONVERTED_URL).unwrap();
        assert_eq!(get_query_sets(&url).unwrap(), get_query_sets(&gen).unwrap());
        url.set_fragment(None);
        url.set_query(None);
        gen.set_fragment(None);
        gen.set_query(None);
        assert_eq!(url, gen);
    }

    #[test]
    fn new_virtual_element_call_widget_id() {
        assert_eq!(get_widget_settings().id(), WIDGET_ID);
    }

    #[test]
    fn new_virtual_element_call_widget_webview_url() {
        const CONVERTED_URL: &str = "https://call.element.io/room#?\
        parentUrl=https%3A%2F%2Fcall.element.io&widgetId=1/@#w23\
        &userId=%40test%3Auser.org&deviceId=ABCDEFG&roomId=%21room_id%3Aroom.org\
        &lang=en-US&theme=light\
        &baseUrl=https%3A%2F%2Fclient-matrix.server.org%2F&hideHeader=true\
        &preload=true&skipLobby=false&confineToRoom=true&\
        displayname=hello&avatarUrl=some-url\
        &appPrompt=true&clientId=io.my_matrix.client";

        let gen = get_widget_settings()
            ._generate_webview_url(
                get_profile::v3::Response::new(Some("some-url".into()), Some("hello".into())),
                "@test:user.org".try_into().unwrap(),
                "!room_id:room.org".try_into().unwrap(),
                "ABCDEFG".into(),
                "https://client-matrix.server.org".try_into().unwrap(),
                ClientProperties::new(
                    "io.my_matrix.client",
                    Some(language_tags::LanguageTag::parse("en-us").unwrap()),
                    Some("light".into()),
                ),
            )
            .unwrap()
            .to_string();
        let mut url = Url::parse(&gen).unwrap();
        let mut gen = Url::parse(CONVERTED_URL).unwrap();
        assert_eq!(get_query_sets(&url).unwrap(), get_query_sets(&gen).unwrap());
        url.set_fragment(None);
        url.set_query(None);
        gen.set_fragment(None);
        gen.set_query(None);
        assert_eq!(url, gen);
    }
}
