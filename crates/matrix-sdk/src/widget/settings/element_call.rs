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

// This module contains ALL the Element Call related code (minus the FFI
// bindings for this file). Hence all other files in the rust sdk contain code
// that is relevant for all widgets. This makes it simple to rip out Element
// Call related pieces.
// TODO: The goal is to have not any Element Call specific code
// in the rust sdk. Find a better solution for this.

use serde::Serialize;
use url::Url;

use super::{url_params, WidgetSettings};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ElementCallParams {
    user_id: String,
    room_id: String,
    widget_id: String,
    display_name: String,
    lang: String,
    theme: String,
    client_id: String,
    device_id: String,
    base_url: String,
    // Non template parameters
    parent_url: String,
    skip_lobby: bool,
    confine_to_room: bool,
    app_prompt: bool,
    hide_header: bool,
    preload: bool,
    analytics_id: Option<String>,
    font_scale: Option<f64>,
    font: Option<String>,
}

/// Properties to create a new virtual Element Call widget.
#[derive(Debug)]
pub struct VirtualElementCallWidgetOptions {
    /// The url to the app.
    ///
    /// E.g. <https://call.element.io>, <https://call.element.dev>
    pub element_call_url: String,

    /// The widget id.
    pub widget_id: String,

    /// The url that is used as the target for the PostMessages sent
    /// by the widget (to the client).
    ///
    /// For a web app client this is the client url. In case of using other
    /// platforms the client most likely is setup up to listen to
    /// postmessages in the same webview the widget is hosted. In this case
    /// the `parent_url` is set to the url of the webview with the widget. Be
    /// aware that this means that the widget will receive its own postmessage
    /// messages. The `matrix-widget-api` (js) ignores those so this works but
    /// it might break custom implementations.
    ///
    /// Defaults to `element_call_url` for the non-iframe (dedicated webview)
    /// usecase.
    pub parent_url: Option<String>,

    /// Whether the branding header of Element call should be hidden.
    ///
    /// Default: `true`
    pub hide_header: Option<bool>,

    /// If set, the lobby will be skipped and the widget will join the
    /// call on the `io.element.join` action.
    ///
    /// Default: `false`
    pub preload: Option<bool>,

    /// The font scale which will be used inside element call.
    ///
    /// Default: `1`
    pub font_scale: Option<f64>,

    /// Whether element call should prompt the user to open in the browser or
    /// the app.
    ///
    /// Default: `false`
    pub app_prompt: Option<bool>,

    /// Don't show the lobby and join the call immediately.
    ///
    /// Default: `false`
    pub skip_lobby: Option<bool>,

    /// Make it not possible to get to the calls list in the webview.
    ///
    /// Default: `true`
    pub confine_to_room: Option<bool>,

    /// The font to use, to adapt to the system font.
    pub font: Option<String>,

    /// Can be used to pass a PostHog id to element call.
    pub analytics_id: Option<String>,
}

impl WidgetSettings {
    /// `WidgetSettings` are usually created from a state event.
    /// (currently unimplemented)
    ///
    /// In some cases the client wants to create custom `WidgetSettings`
    /// for specific rooms based on other conditions.
    /// This function returns a `WidgetSettings` object which can be used
    /// to setup a widget using `run_client_widget_api`
    /// and to generate the correct url for the widget.
    ///
    /// # Arguments
    ///
    /// * - `props` A struct containing the configuration parameters for a
    /// element call widget.
    pub fn new_virtual_element_call_widget(
        props: VirtualElementCallWidgetOptions,
    ) -> Result<Self, url::ParseError> {
        let mut raw_url: Url = Url::parse(&format!("{}/room", props.element_call_url))?;

        let query_params = ElementCallParams {
            user_id: url_params::USER_ID.to_owned(),
            room_id: url_params::ROOM_ID.to_owned(),
            widget_id: url_params::WIDGET_ID.to_owned(),
            display_name: url_params::DISPLAY_NAME.to_owned(),
            lang: url_params::LANGUAGE.to_owned(),
            theme: url_params::CLIENT_THEME.to_owned(),
            client_id: url_params::CLIENT_ID.to_owned(),
            device_id: url_params::DEVICE_ID.to_owned(),
            base_url: url_params::HOMESERVER_URL.to_owned(),

            parent_url: props.parent_url.unwrap_or(props.element_call_url.clone()),
            skip_lobby: props.skip_lobby.unwrap_or(false),
            confine_to_room: props.confine_to_room.unwrap_or(true),
            app_prompt: props.app_prompt.unwrap_or(false),
            hide_header: props.hide_header.unwrap_or(true),
            preload: props.preload.unwrap_or(false),
            analytics_id: props.analytics_id,
            font_scale: props.font_scale,
            font: props.font,
        };

        let query =
            serde_html_form::to_string(query_params).map_err(|_| url::ParseError::Overflow)?;

        // Revert the encoding for the template parameters. So we can have a unified
        // replace logic.
        let query = query.replace("%24", "$");

        // All the params will be set inside the fragment (to keep the traffic to the
        // server minimal and most importantly don't send the passwords).
        raw_url.set_fragment(Some(&format!("?{}", query)));

        // for EC we always want init on content load to be true.
        Ok(Self { id: props.widget_id, init_on_content_load: true, raw_url })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use ruma::api::client::profile::get_profile;
    use url::Url;

    use crate::widget::{ClientProperties, WidgetSettings};

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
            font: None,
            analytics_id: None,
        })
        .expect("could not parse virtual element call widget")
    }

    trait FragmentQuery {
        fn fragment_query(&self) -> Option<&str>;
    }

    impl FragmentQuery for Url {
        fn fragment_query(&self) -> Option<&str> {
            Some(self.fragment()?.split_once('?')?.1)
        }
    }

    // Convert query strings to BTreeSet so that we can compare the urls independent
    // of the order of the params.
    type QuerySet = BTreeSet<(String, String)>;

    use serde_html_form::from_str;

    use super::VirtualElementCallWidgetOptions;

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
                &displayName=$matrix_display_name\
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
                &displayName=hello\
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
