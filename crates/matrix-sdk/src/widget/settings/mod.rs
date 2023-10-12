// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub use element_call::VirtualElementCallWidgetOptions;
use language_tags::LanguageTag;
use ruma::{api::client::profile::get_profile, DeviceId, RoomId, UserId};
use url::Url;

use crate::Room;

mod element_call;
mod url_params;

/// Settings of the widget.
#[derive(Debug, Clone)]
pub struct WidgetSettings {
    id: String,
    init_on_content_load: bool,
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
    pub fn init_on_content_load(&self) -> bool {
        self.init_on_content_load
    }

    /// This contains the url from the widget state event.
    /// In this url placeholders can be used to pass information from the client
    /// to the widget. Possible values are: `$matrix_widget_id`,
    /// `$matrix_display_name`, etc.
    ///
    /// # Examples
    ///
    /// `http://widget.domain?username=$userId` will become
    /// `http://widget.domain?username=@user_matrix_id:server.domain`.
    pub fn raw_url(&self) -> &Url {
        &self.raw_url
    }

    /// Get the base url of the widget. Used as the target for PostMessages. In
    /// case the widget is in a webview and not an IFrame. It contains the
    /// schema and the authority e.g. `https://my.domain.org`. A postmessage would
    /// be sent using: `postMessage(myMessage, widget_base_url)`.
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
    //
    // TODO: add `From<WidgetStateEvent>`, so that `WidgetSettings` can be built
    // by using the room state.
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
        init_on_content_load: bool,
        raw_url: &str,
    ) -> Result<Self, url::ParseError> {
        Ok(Self { id, init_on_content_load, raw_url: Url::parse(raw_url)? })
    }
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
    /// Creates client properties. If a malformatted language tag is provided,
    /// the default one (en-US) will be used.
    ///
    /// # Arguments
    /// * `client_id` - client identifier. This allows widgets to adapt to
    ///   specific clients (e.g. `io.element.web`).
    /// * `language` - language that is used in the client (default: `en-US`).
    /// * `theme` - theme (dark, light) or org.example.dark (default: `light`).
    pub fn new(client_id: &str, language: Option<LanguageTag>, theme: Option<String>) -> Self {
        // It is safe to unwrap "en-us".
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
    url.path_segments_mut().ok()?.clear();
    url.set_query(None);
    url.set_fragment(None);
    Some(url)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use ruma::api::client::profile::get_profile;
    use url::Url;

    use super::{element_call::VirtualElementCallWidgetOptions, WidgetSettings};
    use crate::widget::ClientProperties;

    const WIDGET_ID: &str = "1/@#w23";

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
    fn new_virtual_element_call_widget_base_url() {
        let widget_settings = get_widget_settings();
        assert_eq!(widget_settings.base_url().unwrap().as_str(), "https://call.element.io/");
    }

    #[test]
    fn new_virtual_element_call_widget_raw_url() {
        const CONVERTED_URL: &str = "
            https://call.element.io/room#\
                ?userId=$matrix_user_id\
                &roomId=$matrix_room_id\
                &widgetId=$matrix_widget_id\
                &avatarUrl=$matrix_avatar_url\
                &displayname=$matrix_display_name\
                &lang=$org.matrix.msc2873.client_language\
                &theme=$org.matrix.msc2873.client_theme\
                &clientId=$org.matrix.msc2873.client_id\
                &deviceId=$org.matrix.msc2873.matrix_device_id\
                &baseUrl=$org.matrix.msc4039.matrix_base_url\
                &parentUrl=https%3A%2F%2Fcall.element.io\
                &skipLobby=false\
                &confineToRoom=true\
                &appPrompt=true\
                &hideHeader=true\
                &preload=true\
        ";

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
        const CONVERTED_URL: &str = "
            https://call.element.io/room#\
                ?parentUrl=https%3A%2F%2Fcall.element.io\
                &widgetId=1/@#w23\
                &userId=%40test%3Auser.org&deviceId=ABCDEFG\
                &roomId=%21room_id%3Aroom.org\
                &lang=en-US&theme=light\
                &baseUrl=https%3A%2F%2Fclient-matrix.server.org%2F\
                &hideHeader=true\
                &preload=true\
                &skipLobby=false\
                &confineToRoom=true\
                &displayname=hello\
                &avatarUrl=some-url\
                &appPrompt=true\
                &clientId=io.my_matrix.client\
        ";

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
