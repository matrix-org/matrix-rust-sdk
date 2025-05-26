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
/// Parameters for the Element Call widget.
/// These are documented at https://github.com/element-hq/element-call/blob/livekit/docs/url-params.md
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
    /// Deprecated since Element Call v0.8.0. Included for backwards
    /// compatibility. Set to `true` if intent is `Intent::StartCall`.
    skip_lobby: Option<bool>,
    confine_to_room: bool,
    app_prompt: bool,
    hide_header: bool,
    preload: bool,
    /// Deprecated since Element Call v0.9.0. Included for backwards
    /// compatibility. Set to the same as `posthog_user_id`.
    analytics_id: Option<String>,
    /// Supported since Element Call v0.9.0.
    posthog_user_id: Option<String>,
    font_scale: Option<f64>,
    font: Option<String>,
    #[serde(rename = "perParticipantE2EE")]
    per_participant_e2ee: bool,
    password: Option<String>,
    /// Supported since Element Call v0.8.0.
    intent: Option<Intent>,
    /// Supported since Element Call v0.9.0. Only used by the embedded package.
    posthog_api_host: Option<String>,
    /// Supported since Element Call v0.9.0. Only used by the embedded package.
    posthog_api_key: Option<String>,
    /// Supported since Element Call v0.9.0. Only used by the embedded package.
    rageshake_submit_url: Option<String>,
    /// Supported since Element Call v0.9.0. Only used by the embedded package.
    sentry_dsn: Option<String>,
    /// Supported since Element Call v0.9.0. Only used by the embedded package.
    sentry_environment: Option<String>,
    hide_screensharing: bool,
    controlled_media_devices: bool,
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
    /// This is only used by the embedded package of Element Call.
    pub posthog_api_host: Option<String>,
    /// The key for the posthog api.
    /// This is only used by the embedded package of Element Call.
    pub posthog_api_key: Option<String>,

    /// The url to use for submitting rageshakes.
    /// This is only used by the embedded package of Element Call.
    pub rageshake_submit_url: Option<String>,

    /// Sentry [DSN](https://docs.sentry.io/concepts/key-terms/dsn-explainer/)
    /// This is only used by the embedded package of Element Call.
    pub sentry_dsn: Option<String>,
    /// Sentry [environment](https://docs.sentry.io/concepts/key-terms/key-terms/)
    /// This is only used by the embedded package of Element Call.
    pub sentry_environment: Option<String>,
    //// - `true`: The webview should show the list of media devices it detects using
    ////   `enumerateDevices`.
    ///  - `false`: the webview shows a a list of devices injected by the
    ///    client. (used on ios & android)
    pub controlled_media_devices: bool,
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

        let skip_lobby = if props.intent.as_ref().is_some_and(|x| x == &Intent::StartCall) {
            Some(true)
        } else {
            None
        };

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
            font_scale: props.font_scale,
            font: props.font,
            per_participant_e2ee: props.encryption == EncryptionSystem::PerParticipantKeys,
            password: match props.encryption {
                EncryptionSystem::SharedSecret { secret } => Some(secret),
                _ => None,
            },
            intent: props.intent,
            skip_lobby,
            analytics_id: props.posthog_user_id.clone(),
            posthog_user_id: props.posthog_user_id,
            posthog_api_host: props.posthog_api_host,
            posthog_api_key: props.posthog_api_key,
            sentry_dsn: props.sentry_dsn,
            sentry_environment: props.sentry_environment,
            rageshake_submit_url: props.rageshake_submit_url,
            hide_screensharing: props.hide_screensharing,
            controlled_media_devices: props.controlled_media_devices,
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

    use crate::widget::{ClientProperties, Intent, WidgetSettings};

    const WIDGET_ID: &str = "1/@#w23";

    fn get_widget_settings(
        encryption: Option<EncryptionSystem>,
        posthog: bool,
        rageshake: bool,
        sentry: bool,
        intent: Option<Intent>,
        controlle_output: bool,
    ) -> WidgetSettings {
        let mut props = VirtualElementCallWidgetOptions {
            element_call_url: "https://call.element.io".to_owned(),
            widget_id: WIDGET_ID.to_owned(),
            hide_header: Some(true),
            preload: Some(true),
            app_prompt: Some(true),
            confine_to_room: Some(true),
            encryption: encryption.unwrap_or(EncryptionSystem::PerParticipantKeys),
            intent,
            controlled_media_devices: controlle_output,
            ..VirtualElementCallWidgetOptions::default()
        };

        if posthog {
            props.posthog_user_id = Some("POSTHOG_USER_ID".to_owned());
            props.posthog_api_host = Some("posthog.element.io".to_owned());
            props.posthog_api_key = Some("POSTHOG_KEY".to_owned());
        }

        if rageshake {
            props.rageshake_submit_url = Some("https://rageshake.element.io".to_owned());
        }

        if sentry {
            props.sentry_dsn = Some("SENTRY_DSN".to_owned());
            props.sentry_environment = Some("SENTRY_ENV".to_owned());
        }

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
    fn test_new_virtual_element_call_widget_base_url() {
        let widget_settings = get_widget_settings(None, false, false, false, None, false);
        assert_eq!(widget_settings.base_url().unwrap().as_str(), "https://call.element.io/");
    }

    #[test]
    fn test_new_virtual_element_call_widget_raw_url() {
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
                &controlledMediaDevices=false\
        ";

        let mut url = get_widget_settings(None, false, false, false, None, false).raw_url().clone();
        let mut gen = Url::parse(CONVERTED_URL).unwrap();
        assert_eq!(get_query_sets(&url).unwrap(), get_query_sets(&gen).unwrap());
        url.set_fragment(None);
        url.set_query(None);
        gen.set_fragment(None);
        gen.set_query(None);
        assert_eq!(url, gen);
    }

    #[test]
    fn test_new_virtual_element_call_widget_id() {
        assert_eq!(
            get_widget_settings(None, false, false, false, None, false).widget_id(),
            WIDGET_ID
        );
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
    fn test_new_virtual_element_call_widget_webview_url() {
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
                &controlledMediaDevices=false\
        ";
        let gen = build_url_from_widget_settings(get_widget_settings(
            None, false, false, false, None, false,
        ));

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
    fn test_new_virtual_element_call_widget_webview_url_with_posthog_rageshake_sentry() {
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
                &controlledMediaDevices=false\
        ";
        let gen = build_url_from_widget_settings(get_widget_settings(
            None, true, true, true, None, false,
        ));

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
    fn test_password_url_props_from_widget_settings() {
        {
            // PerParticipantKeys
            let url = build_url_from_widget_settings(get_widget_settings(
                Some(EncryptionSystem::PerParticipantKeys),
                false,
                false,
                false,
                None,
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
                None,
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
                None,
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

    #[test]
    fn test_controlled_output_url_props_from_widget_settings() {
        {
            // PerParticipantKeys
            let url = build_url_from_widget_settings(get_widget_settings(
                Some(EncryptionSystem::PerParticipantKeys),
                false,
                false,
                false,
                None,
                true,
            ));
            let controlled_media_element = ("controlledMediaDevices".to_owned(), "true".to_owned());
            let query_set = get_query_sets(&Url::parse(&url).unwrap()).unwrap().1;
            assert!(
                query_set.contains(&controlled_media_element),
                "The query elements: \n{:?}\nDid not contain: \n{:?}",
                query_set,
                controlled_media_element
            );
        }
    }

    #[test]
    fn test_intent_url_props_from_widget_settings() {
        {
            // no intent
            let url = build_url_from_widget_settings(get_widget_settings(
                None, false, false, false, None, false,
            ));
            let query_set = get_query_sets(&Url::parse(&url).unwrap()).unwrap().1;

            let expected_unset_elements = ["intent".to_owned(), "skipLobby".to_owned()];

            for e in expected_unset_elements {
                assert!(
                    !query_set.iter().any(|x| x.0 == e),
                    "The query elements: \n{:?}\nShould not have contained: \n{:?}",
                    query_set,
                    e
                );
            }
        }
        {
            // Intent::JoinExisting
            let url = build_url_from_widget_settings(get_widget_settings(
                None,
                false,
                false,
                false,
                Some(Intent::JoinExisting),
                false,
            ));
            let query_set = get_query_sets(&Url::parse(&url).unwrap()).unwrap().1;
            let expected_elements = ("intent".to_owned(), "join_existing".to_owned());
            assert!(
                query_set.contains(&expected_elements),
                "The url query elements for an unencrypted call: \n{:?}\nDid not contain: \n{:?}",
                query_set,
                expected_elements
            );

            let expected_unset_elements = ["skipLobby".to_owned()];

            for e in expected_unset_elements {
                assert!(
                    !query_set.iter().any(|x| x.0 == e),
                    "The query elements: \n{:?}\nShould not have contained: \n{:?}",
                    query_set,
                    e
                );
            }
        }
        {
            // Intent::StartCall
            let url = build_url_from_widget_settings(get_widget_settings(
                None,
                false,
                false,
                false,
                Some(Intent::StartCall),
                false,
            ));
            let query_set = get_query_sets(&Url::parse(&url).unwrap()).unwrap().1;

            // skipLobby should be set for compatibility with versions < 0.8.0
            let expected_elements = [
                ("intent".to_owned(), "start_call".to_owned()),
                ("skipLobby".to_owned(), "true".to_owned()),
            ];
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
