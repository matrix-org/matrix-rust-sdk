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
    confine_to_room: bool,
    app_prompt: bool,
    hide_header: bool,
    preload: bool,
    /// The same as `posthog_user_id` used for backwards compatibility.
    analytics_id: Option<String>,
    posthog_user_id: Option<String>,
    font_scale: Option<f64>,
    font: Option<String>,
    #[serde(rename = "perParticipantE2EE")]
    per_participant_e2ee: bool,
    password: Option<String>,
    intent: Option<Intent>,
    posthog_api_host: Option<String>,
    posthog_api_key: Option<String>,
    rageshake_submit_url: Option<String>,
    sentry_dsn: Option<String>,
    sentry_environment: Option<String>,
    hide_screensharing: bool,
}

/// Defines if a call is encrypted and which encryption system should be used.
///
/// This controls the url parameters: `perParticipantE2EE`, `password`.
#[derive(Debug, PartialEq, Default)]
pub enum EncryptionSystem {
    /// Equivalent to the element call url parameter: `perParticipantE2EE=false`
    /// and no password.
    Unencrypted,
    /// Equivalent to the element call url parameters:
    /// `perParticipantE2EE=true`
    #[default]
    PerParticipantKeys,
    /// Equivalent to the element call url parameters:
    /// `password={secret}`
    SharedSecret {
        /// The secret/password which is used in the url.
        secret: String,
    },
}

/// Defines the intent of showing the call.
///
/// This controls whether to show or skip the lobby.
#[derive(Debug, PartialEq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    #[default]
    /// The user wants to start a call.
    StartCall,
    /// The user wants to join an existing call.
    JoinExisting,
}

/// Properties to create a new virtual Element Call widget.
#[derive(Debug, Default)]
pub struct VirtualElementCallWidgetOptions {
    /// The url to the app.
    ///
    /// E.g. <https://call.element.io>, <https://call.element.dev>, <https://call.element.dev/room>
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

    /// Make it not possible to get to the calls list in the webview.
    ///
    /// Default: `true`
    pub confine_to_room: Option<bool>,

    /// The font to use, to adapt to the system font.
    pub font: Option<String>,

    /// The encryption system to use.
    ///
    /// Use `EncryptionSystem::Unencrypted` to disable encryption.
    pub encryption: EncryptionSystem,

    /// The intent of showing the call.
    /// If the user wants to start a call or join an existing one.
    /// Controls if the lobby is skipped or not.
    pub intent: Option<Intent>,

    /// Do not show the screenshare button.
    pub hide_screensharing: bool,

    /// Can be used to pass a PostHog id to element call.
    pub posthog_user_id: Option<String>,
    /// The host of the posthog api.
    pub posthog_api_host: Option<String>,
    /// The key for the posthog api.
    pub posthog_api_key: Option<String>,

    /// The url to use for submitting rageshakes.
    pub rageshake_submit_url: Option<String>,

    /// Sentry [DSN](https://docs.sentry.io/concepts/key-terms/dsn-explainer/)
    pub sentry_dsn: Option<String>,
    /// Sentry [environment](https://docs.sentry.io/concepts/key-terms/key-terms/)
    pub sentry_environment: Option<String>,
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
    /// * `props` - A struct containing the configuration parameters for a
    ///   element call widget.
    pub fn new_virtual_element_call_widget(
        props: VirtualElementCallWidgetOptions,
    ) -> Result<Self, url::ParseError> {
        let mut raw_url: Url = Url::parse(&props.element_call_url)?;

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
            confine_to_room: props.confine_to_room.unwrap_or(true),
            app_prompt: props.app_prompt.unwrap_or(false),
            hide_header: props.hide_header.unwrap_or(true),
            preload: props.preload.unwrap_or(false),
            analytics_id: props.posthog_user_id.clone(),
            posthog_user_id: props.posthog_user_id,
            font_scale: props.font_scale,
            font: props.font,
            per_participant_e2ee: props.encryption == EncryptionSystem::PerParticipantKeys,
            password: match props.encryption {
                EncryptionSystem::SharedSecret { secret } => Some(secret),
                _ => None,
            },
            intent: props.intent,
            posthog_api_host: props.posthog_api_host,
            posthog_api_key: props.posthog_api_key,
            rageshake_submit_url: props.rageshake_submit_url,
            sentry_dsn: props.sentry_dsn,
            sentry_environment: props.sentry_environment,
            hide_screensharing: props.hide_screensharing,
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
        Ok(Self { widget_id: props.widget_id, init_on_content_load: true, raw_url })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use ruma::api::client::profile::get_profile;
    use url::Url;

    use crate::widget::{ClientProperties, WidgetSettings};

    const WIDGET_ID: &str = "1/@#w23";

    fn get_widget_settings(
        encryption: Option<EncryptionSystem>,
        posthog: bool,
        rageshake: bool,
        sentry: bool,
    ) -> WidgetSettings {
        let props = VirtualElementCallWidgetOptions {
            element_call_url: "https://call.element.io".to_owned(),
            widget_id: WIDGET_ID.to_owned(),
            hide_header: Some(true),
            preload: Some(true),
            app_prompt: Some(true),
            confine_to_room: Some(true),
            encryption: encryption.unwrap_or(EncryptionSystem::PerParticipantKeys),
            ..VirtualElementCallWidgetOptions::default()
        };

        let props = if posthog {
            VirtualElementCallWidgetOptions {
                posthog_user_id: Some("POSTHOG_USER_ID".to_owned()),
                posthog_api_host: Some("posthog.element.io".to_owned()),
                posthog_api_key: Some("POSTHOG_KEY".to_owned()),
                ..props
            }
        } else {
            props
        };
        let props = if rageshake {
            VirtualElementCallWidgetOptions {
                rageshake_submit_url: Some("https://rageshake.element.io".to_owned()),
                ..props
            }
        } else {
            props
        };
        let props = if sentry {
            VirtualElementCallWidgetOptions {
                sentry_dsn: Some("SENTRY_DSN".to_owned()),
                sentry_environment: Some("SENTRY_ENV".to_owned()),
                ..props
            }
        } else {
            props
        };

        WidgetSettings::new_virtual_element_call_widget(props)
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

    use super::{EncryptionSystem, VirtualElementCallWidgetOptions};

    fn get_query_sets(url: &Url) -> Option<(QuerySet, QuerySet)> {
        let fq = from_str::<QuerySet>(url.fragment_query().unwrap_or_default()).ok()?;
        let q = from_str::<QuerySet>(url.query().unwrap_or_default()).ok()?;
        Some((q, fq))
    }

    #[test]
    fn new_virtual_element_call_widget_base_url() {
        let widget_settings = get_widget_settings(None, false, false, false);
        assert_eq!(widget_settings.base_url().unwrap().as_str(), "https://call.element.io/");
    }

    #[test]
    fn new_virtual_element_call_widget_raw_url() {
        const CONVERTED_URL: &str = "
            https://call.element.io#\
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
                &confineToRoom=true\
                &appPrompt=true\
                &hideHeader=true\
                &preload=true\
                &perParticipantE2EE=true\
                &hideScreensharing=false\
        ";

        let mut url = get_widget_settings(None, false, false, false).raw_url().clone();
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
        assert_eq!(get_widget_settings(None, false, false, false).widget_id(), WIDGET_ID);
    }

    fn build_url_from_widget_settings(settings: WidgetSettings) -> String {
        settings
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
            .to_string()
    }

    #[test]
    fn new_virtual_element_call_widget_webview_url() {
        const CONVERTED_URL: &str = "
            https://call.element.io#\
                ?parentUrl=https%3A%2F%2Fcall.element.io\
                &widgetId=1/@#w23\
                &userId=%40test%3Auser.org&deviceId=ABCDEFG\
                &roomId=%21room_id%3Aroom.org\
                &lang=en-US&theme=light\
                &baseUrl=https%3A%2F%2Fclient-matrix.server.org%2F\
                &hideHeader=true\
                &preload=true\
                &confineToRoom=true\
                &displayName=hello\
                &appPrompt=true\
                &clientId=io.my_matrix.client\
                &perParticipantE2EE=true\
                &hideScreensharing=false\
        ";
        let gen = build_url_from_widget_settings(get_widget_settings(None, false, false, false));

        let mut url = Url::parse(&gen).unwrap();
        let mut gen = Url::parse(CONVERTED_URL).unwrap();
        assert_eq!(get_query_sets(&url).unwrap(), get_query_sets(&gen).unwrap());
        url.set_fragment(None);
        url.set_query(None);
        gen.set_fragment(None);
        gen.set_query(None);
        assert_eq!(url, gen);
    }

    #[test]
    fn new_virtual_element_call_widget_webview_url_with_posthog_rageshake_sentry() {
        const CONVERTED_URL: &str = "
            https://call.element.io#\
                ?parentUrl=https%3A%2F%2Fcall.element.io\
                &widgetId=1/@#w23\
                &userId=%40test%3Auser.org&deviceId=ABCDEFG\
                &roomId=%21room_id%3Aroom.org\
                &lang=en-US&theme=light\
                &baseUrl=https%3A%2F%2Fclient-matrix.server.org%2F\
                &hideHeader=true\
                &preload=true\
                &confineToRoom=true\
                &displayName=hello\
                &appPrompt=true\
                &clientId=io.my_matrix.client\
                &perParticipantE2EE=true\
                &hideScreensharing=false\
                &posthogApiHost=posthog.element.io\
                &posthogApiKey=POSTHOG_KEY\
                &analyticsId=POSTHOG_USER_ID\
                &posthogUserId=POSTHOG_USER_ID\
                &rageshakeSubmitUrl=https%3A%2F%2Frageshake.element.io\
                &sentryDsn=SENTRY_DSN\
                &sentryEnvironment=SENTRY_ENV\
        ";
        let gen = build_url_from_widget_settings(get_widget_settings(None, true, true, true));

        let mut url = Url::parse(&gen).unwrap();
        let mut gen = Url::parse(CONVERTED_URL).unwrap();
        assert_eq!(get_query_sets(&url).unwrap(), get_query_sets(&gen).unwrap());
        url.set_fragment(None);
        url.set_query(None);
        gen.set_fragment(None);
        gen.set_query(None);
        assert_eq!(url, gen);
    }

    #[test]
    fn password_url_props_from_widget_settings() {
        {
            // PerParticipantKeys
            let url = build_url_from_widget_settings(get_widget_settings(
                Some(EncryptionSystem::PerParticipantKeys),
                false,
                false,
                false,
            ));
            let query_set = get_query_sets(&Url::parse(&url).unwrap()).unwrap().1;
            let expected_elements = [("perParticipantE2EE".to_owned(), "true".to_owned())];
            for e in expected_elements {
                assert!(
                    query_set.contains(&e),
                    "The query elements: \n{:?}\nDid not contain: \n{:?}",
                    query_set,
                    e
                );
            }
        }
        {
            // Unencrypted
            let url = build_url_from_widget_settings(get_widget_settings(
                Some(EncryptionSystem::Unencrypted),
                false,
                false,
                false,
            ));
            let query_set = get_query_sets(&Url::parse(&url).unwrap()).unwrap().1;
            let expected_elements = ("perParticipantE2EE".to_owned(), "false".to_owned());
            assert!(
                query_set.contains(&expected_elements),
                "The url query elements for an unencrypted call: \n{:?}\nDid not contain: \n{:?}",
                query_set,
                expected_elements
            );
        }
        {
            // SharedSecret
            let url = build_url_from_widget_settings(get_widget_settings(
                Some(EncryptionSystem::SharedSecret { secret: "this_surely_is_save".to_owned() }),
                false,
                false,
                false,
            ));
            let query_set = get_query_sets(&Url::parse(&url).unwrap()).unwrap().1;
            let expected_elements = [("password".to_owned(), "this_surely_is_save".to_owned())];
            for e in expected_elements {
                assert!(
                    query_set.contains(&e),
                    "The query elements: \n{:?}\nDid not contain: \n{:?}",
                    query_set,
                    e
                );
            }
        }
    }
}
