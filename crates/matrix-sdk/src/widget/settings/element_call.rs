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

use super::{WidgetSettings, url_params};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
/// Serialization struct for URL parameters for the Element Call widget.
/// These are documented at https://github.com/element-hq/element-call/blob/livekit/docs/url-params.md
///
/// The ElementCallParams are used to be translated into url query parameters.
/// For all optional fields, the None case implies, that it will not be part of
/// the url parameters.
///
/// # Example:
///
/// ```
/// ElementCallParams {
///     // Required parameters:
///     user_id: "@1234",
///     room_id: "$1234",
///     ...
///     // Optional configuration:
///     hide_screensharing: Some(true),
///     ..ElementCallParams::default()
/// }
/// ```
/// will become: `my.url? ...requires_parameters... &hide_screensharing=true`
/// The reason it might be desirable to not list those configurations in the
/// URLs parameters is that the `intent` implies defaults for all configuration
/// values in the widget itself. Setting the URL parameter specifically will
/// overwrite those defaults.
struct ElementCallUrlParams {
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
    confine_to_room: Option<bool>,
    app_prompt: Option<bool>,
    /// Supported since Element Call v0.13.0.
    header: Option<HeaderStyle>,
    /// Deprecated since Element Call v0.13.0. Included for backwards
    /// compatibility. Use header: "standard"|"none" instead.
    hide_header: Option<bool>,
    preload: Option<bool>,
    /// Deprecated since Element Call v0.9.0. Included for backwards
    /// compatibility. Set to the same as `posthog_user_id`.
    analytics_id: Option<String>,
    /// Supported since Element Call v0.9.0.
    posthog_user_id: Option<String>,
    font_scale: Option<f64>,
    font: Option<String>,
    #[serde(rename = "perParticipantE2EE")]
    per_participant_e2ee: Option<bool>,
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
    /// Supported since Element Call v0.9.0.
    hide_screensharing: Option<bool>,
    /// Supported since Element Call v0.13.0.
    controlled_audio_devices: Option<bool>,
    /// Supported since Element Call v0.14.0.
    send_notification_type: Option<NotificationType>,
}

/// Defines if a call is encrypted and which encryption system should be used.
///
/// This controls the url parameters: `perParticipantE2EE`, `password`.
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug, PartialEq, Default, Clone)]
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
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug, PartialEq, Serialize, Default, Clone)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    #[default]
    /// The user wants to start a call.
    StartCall,
    /// The user wants to join an existing call.
    JoinExisting,
    /// The user wants to join an existing call that is a "Direct Message" (DM)
    /// room.
    JoinExistingDm,
    /// The user wants to start a call in a "Direct Message" (DM) room.
    StartCallDm,
    /// The user wants to start a voice call in a "Direct Message" (DM) room.
    StartCallDmVoice,
    /// The user wants to join an existing  voice call that is a "Direct
    /// Message" (DM) room.
    JoinExistingDmVoice,
}

/// Defines how (if) element-call renders a header.
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug, PartialEq, Serialize, Default, Clone)]
#[serde(rename_all = "snake_case")]
pub enum HeaderStyle {
    /// The normal header with branding.
    #[default]
    Standard,
    /// Render a header with a back button (useful on mobile platforms).
    AppBar,
    /// No Header (useful for webapps).
    None,
}

/// Types of call notifications.
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug, PartialEq, Serialize, Clone, Default)]
#[serde(rename_all = "snake_case")]
pub enum NotificationType {
    /// The receiving client should display a visual notification.
    #[default]
    Notification,
    /// The receiving client should ring with an audible sound.
    Ring,
}

/// Configuration parameters, to create a new virtual Element Call widget.
///
/// If `intent` is provided the appropriate default values for all other
/// parameters will be used by element call.
/// In most cases its enough to only set the intent. Use the other properties
/// only if you want to deviate from the `intent` defaults.
///
/// Set [`docs/url-params.md`](https://github.com/element-hq/element-call/blob/livekit/docs/url-params.md)
/// to find out more about the parameters and their defaults.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Default, Clone)]
pub struct VirtualElementCallWidgetConfig {
    /// The intent of showing the call.
    /// If the user wants to start a call or join an existing one.
    /// Controls if the lobby is skipped or not.
    pub intent: Option<Intent>,

    /// Skip the lobby when joining a call.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub skip_lobby: Option<bool>,

    /// Whether the branding header of Element call should be shown or if a
    /// mobile header navbar should be render.
    ///
    /// Default: [`HeaderStyle::Standard`]
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub header: Option<HeaderStyle>,

    /// Whether the branding header of Element call should be hidden.
    ///
    /// Default: `true`
    #[deprecated(note = "Use `header` instead", since = "0.12.1")]
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub hide_header: Option<bool>,

    /// If set, the lobby will be skipped and the widget will join the
    /// call on the `io.element.join` action.
    ///
    /// Default: `false`
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub preload: Option<bool>,

    /// Whether element call should prompt the user to open in the browser or
    /// the app.
    ///
    /// Default: `false`
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub app_prompt: Option<bool>,

    /// Make it not possible to get to the calls list in the webview.
    ///
    /// Default: `true`
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub confine_to_room: Option<bool>,

    /// Do not show the screenshare button.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub hide_screensharing: Option<bool>,

    /// Make the audio devices be controlled by the os instead of the
    /// element-call webview.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub controlled_audio_devices: Option<bool>,

    /// Whether and what type of notification Element Call should send, when
    /// starting a call.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub send_notification_type: Option<NotificationType>,
}

/// Properties to create a new virtual Element Call widget.
///
/// All these are required to start the widget in the first place.
/// This is different from the `VirtualElementCallWidgetConfiguration` which
/// configures the widgets behavior.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Default, Clone)]
pub struct VirtualElementCallWidgetProperties {
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
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub parent_url: Option<String>,

    /// The font scale which will be used inside element call.
    ///
    /// Default: `1`
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub font_scale: Option<f64>,

    /// The font to use, to adapt to the system font.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub font: Option<String>,

    /// The encryption system to use.
    ///
    /// Use `EncryptionSystem::Unencrypted` to disable encryption.
    pub encryption: EncryptionSystem,

    /// Can be used to pass a PostHog id to element call.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub posthog_user_id: Option<String>,
    /// The host of the posthog api.
    /// This is only used by the embedded package of Element Call.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub posthog_api_host: Option<String>,
    /// The key for the posthog api.
    /// This is only used by the embedded package of Element Call.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub posthog_api_key: Option<String>,

    /// The url to use for submitting rageshakes.
    /// This is only used by the embedded package of Element Call.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub rageshake_submit_url: Option<String>,

    /// Sentry [DSN](https://docs.sentry.io/concepts/key-terms/dsn-explainer/)
    /// This is only used by the embedded package of Element Call.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub sentry_dsn: Option<String>,

    /// Sentry [environment](https://docs.sentry.io/concepts/key-terms/key-terms/)
    /// This is only used by the embedded package of Element Call.
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
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
        props: VirtualElementCallWidgetProperties,
        config: VirtualElementCallWidgetConfig,
    ) -> Result<Self, url::ParseError> {
        let mut raw_url: Url = Url::parse(&props.element_call_url)?;

        #[allow(deprecated)]
        let query_params = ElementCallUrlParams {
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
            confine_to_room: config.confine_to_room,
            app_prompt: config.app_prompt,
            header: config.header,
            hide_header: config.hide_header,
            preload: config.preload,
            font_scale: props.font_scale,
            font: props.font,
            per_participant_e2ee: Some(props.encryption == EncryptionSystem::PerParticipantKeys),
            password: match props.encryption {
                EncryptionSystem::SharedSecret { secret } => Some(secret),
                _ => None,
            },
            intent: config.intent,
            skip_lobby: config.skip_lobby,
            analytics_id: props.posthog_user_id.clone(),
            posthog_user_id: props.posthog_user_id,
            posthog_api_host: props.posthog_api_host,
            posthog_api_key: props.posthog_api_key,
            sentry_dsn: props.sentry_dsn,
            sentry_environment: props.sentry_environment,
            rageshake_submit_url: props.rageshake_submit_url,
            hide_screensharing: config.hide_screensharing,
            controlled_audio_devices: config.controlled_audio_devices,
            send_notification_type: config.send_notification_type,
        };

        let query =
            serde_html_form::to_string(query_params).map_err(|_| url::ParseError::Overflow)?;

        // Revert the encoding for the template parameters. So we can have a unified
        // replace logic.
        let query = query.replace("%24", "$");

        // All the params will be set inside the fragment (to keep the traffic to the
        // server minimal and most importantly don't send the passwords).
        raw_url.set_fragment(Some(&format!("?{query}")));

        // for EC we always want init on content load to be true.
        Ok(Self { widget_id: props.widget_id, init_on_content_load: true, raw_url })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use ruma::api::client::profile::get_profile;
    use url::Url;

    use crate::widget::{
        ClientProperties, Intent, WidgetSettings,
        settings::element_call::{HeaderStyle, VirtualElementCallWidgetConfig},
    };

    const WIDGET_ID: &str = "1/@#w23";

    fn get_element_call_widget_settings(
        encryption: Option<EncryptionSystem>,
        posthog: bool,
        rageshake: bool,
        sentry: bool,
        intent: Option<Intent>,
        controlled_output: bool,
    ) -> WidgetSettings {
        let props = VirtualElementCallWidgetProperties {
            element_call_url: "https://call.element.io".to_owned(),
            widget_id: WIDGET_ID.to_owned(),
            posthog_user_id: posthog.then(|| "POSTHOG_USER_ID".to_owned()),
            posthog_api_host: posthog.then(|| "posthog.element.io".to_owned()),
            posthog_api_key: posthog.then(|| "POSTHOG_KEY".to_owned()),
            rageshake_submit_url: rageshake.then(|| "https://rageshake.element.io".to_owned()),
            sentry_dsn: sentry.then(|| "SENTRY_DSN".to_owned()),
            sentry_environment: sentry.then(|| "SENTRY_ENV".to_owned()),
            encryption: encryption.unwrap_or(EncryptionSystem::PerParticipantKeys),
            ..VirtualElementCallWidgetProperties::default()
        };

        let config = VirtualElementCallWidgetConfig {
            controlled_audio_devices: Some(controlled_output),
            preload: Some(true),
            app_prompt: Some(true),
            confine_to_room: Some(true),
            hide_screensharing: Some(false),
            header: Some(HeaderStyle::Standard),
            intent,
            ..VirtualElementCallWidgetConfig::default()
        };

        WidgetSettings::new_virtual_element_call_widget(props, config)
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

    use super::{EncryptionSystem, VirtualElementCallWidgetProperties};

    fn get_query_sets(url: &Url) -> Option<(QuerySet, QuerySet)> {
        let fq = from_str::<QuerySet>(url.fragment_query().unwrap_or_default()).ok()?;
        let q = from_str::<QuerySet>(url.query().unwrap_or_default()).ok()?;
        Some((q, fq))
    }

    #[test]
    fn test_new_virtual_element_call_widget_base_url() {
        let widget_settings =
            get_element_call_widget_settings(None, false, false, false, None, false);
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
                &header=standard\
                &preload=true\
                &perParticipantE2EE=true\
                &hideScreensharing=false\
                &controlledAudioDevices=false\
        ";

        let mut generated_url =
            get_element_call_widget_settings(None, false, false, false, None, false)
                .raw_url()
                .clone();
        let mut expected_url = Url::parse(CONVERTED_URL).unwrap();
        assert_eq!(get_query_sets(&generated_url).unwrap(), get_query_sets(&expected_url).unwrap());
        generated_url.set_fragment(None);
        generated_url.set_query(None);
        expected_url.set_fragment(None);
        expected_url.set_query(None);
        assert_eq!(generated_url, expected_url);
    }

    #[test]
    fn test_new_virtual_element_call_widget_id() {
        assert_eq!(
            get_element_call_widget_settings(None, false, false, false, None, false).widget_id(),
            WIDGET_ID
        );
    }

    fn build_url_from_widget_settings(settings: WidgetSettings) -> String {
        let mut profile = get_profile::v3::Response::new();
        profile.set("avatar_url".to_owned(), "some-url".into());
        profile.set("displayname".to_owned(), "hello".into());

        settings
            ._generate_webview_url(
                profile,
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
                &header=standard\
                &preload=true\
                &confineToRoom=true\
                &displayName=hello\
                &appPrompt=true\
                &clientId=io.my_matrix.client\
                &perParticipantE2EE=true\
                &hideScreensharing=false\
                &controlledAudioDevices=false\
        ";
        let mut generated_url = Url::parse(&build_url_from_widget_settings(
            get_element_call_widget_settings(None, false, false, false, None, false),
        ))
        .unwrap();
        let mut expected_url = Url::parse(CONVERTED_URL).unwrap();
        assert_eq!(get_query_sets(&generated_url).unwrap(), get_query_sets(&expected_url).unwrap());
        generated_url.set_fragment(None);
        generated_url.set_query(None);
        expected_url.set_fragment(None);
        expected_url.set_query(None);
        assert_eq!(generated_url, expected_url);
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
                &header=standard\
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
                &controlledAudioDevices=false\
        ";
        let mut generated_url = Url::parse(&build_url_from_widget_settings(
            get_element_call_widget_settings(None, true, true, true, None, false),
        ))
        .unwrap();
        let mut original_url = Url::parse(CONVERTED_URL).unwrap();
        assert_eq!(get_query_sets(&generated_url).unwrap(), get_query_sets(&original_url).unwrap());
        generated_url.set_fragment(None);
        generated_url.set_query(None);
        original_url.set_fragment(None);
        original_url.set_query(None);
        assert_eq!(generated_url, original_url);
    }

    #[test]
    fn test_password_url_props_from_widget_settings() {
        {
            // PerParticipantKeys
            let url = build_url_from_widget_settings(get_element_call_widget_settings(
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
                    "The query elements: \n{query_set:?}\nDid not contain: \n{e:?}"
                );
            }
        }
        {
            // Unencrypted
            let url = build_url_from_widget_settings(get_element_call_widget_settings(
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
                "The url query elements for an unencrypted call: \n{query_set:?}\nDid not contain: \n{expected_elements:?}"
            );
        }
        {
            // SharedSecret
            let url = build_url_from_widget_settings(get_element_call_widget_settings(
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
                    "The query elements: \n{query_set:?}\nDid not contain: \n{e:?}"
                );
            }
        }
    }

    #[test]
    fn test_controlled_output_url_props_from_widget_settings() {
        {
            // PerParticipantKeys
            let url = build_url_from_widget_settings(get_element_call_widget_settings(
                Some(EncryptionSystem::PerParticipantKeys),
                false,
                false,
                false,
                None,
                true,
            ));
            let controlled_audio_element = ("controlledAudioDevices".to_owned(), "true".to_owned());
            let query_set = get_query_sets(&Url::parse(&url).unwrap()).unwrap().1;
            assert!(
                query_set.contains(&controlled_audio_element),
                "The query elements: \n{query_set:?}\nDid not contain: \n{controlled_audio_element:?}"
            );
        }
    }

    #[test]
    fn test_intent_url_props_from_widget_settings() {
        {
            // no intent
            let url = build_url_from_widget_settings(get_element_call_widget_settings(
                None, false, false, false, None, false,
            ));
            let query_set = get_query_sets(&Url::parse(&url).unwrap()).unwrap().1;

            let expected_unset_elements = ["intent".to_owned(), "skipLobby".to_owned()];

            for e in expected_unset_elements {
                assert!(
                    !query_set.iter().any(|x| x.0 == e),
                    "The query elements: \n{query_set:?}\nShould not have contained: \n{e:?}"
                );
            }
        }
        {
            // Intent::JoinExisting
            let url = build_url_from_widget_settings(get_element_call_widget_settings(
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
                "The url query elements for an unencrypted call: \n{query_set:?}\nDid not contain: \n{expected_elements:?}"
            );

            let expected_unset_elements = ["skipLobby".to_owned()];

            for e in expected_unset_elements {
                assert!(
                    !query_set.iter().any(|x| x.0 == e),
                    "The query elements: \n{query_set:?}\nShould not have contained: \n{e:?}"
                );
            }
        }
        {
            // Intent::StartCall
            let url = build_url_from_widget_settings(get_element_call_widget_settings(
                None,
                false,
                false,
                false,
                Some(Intent::StartCall),
                false,
            ));
            let query_set = get_query_sets(&Url::parse(&url).unwrap()).unwrap().1;

            let expected_elements = [("intent".to_owned(), "start_call".to_owned())];
            for e in expected_elements {
                assert!(
                    query_set.contains(&e),
                    "The query elements: \n{query_set:?}\nDid not contain: \n{e:?}"
                );
            }
        }
        {
            // Intent::StartCallDm
            let url = build_url_from_widget_settings(get_element_call_widget_settings(
                None,
                false,
                false,
                false,
                Some(Intent::StartCallDm),
                false,
            ));
            let query_set = get_query_sets(&Url::parse(&url).unwrap()).unwrap().1;

            let expected_elements = [("intent".to_owned(), "start_call_dm".to_owned())];
            for e in expected_elements {
                assert!(
                    query_set.contains(&e),
                    "The query elements: \n{query_set:?}\nDid not contain: \n{e:?}"
                );
            }
        }
        {
            // Intent::JoinExistingDm
            let url = build_url_from_widget_settings(get_element_call_widget_settings(
                None,
                false,
                false,
                false,
                Some(Intent::JoinExistingDm),
                false,
            ));
            let query_set = get_query_sets(&Url::parse(&url).unwrap()).unwrap().1;

            let expected_elements = [("intent".to_owned(), "join_existing_dm".to_owned())];
            for e in expected_elements {
                assert!(
                    query_set.contains(&e),
                    "The query elements: \n{query_set:?}\nDid not contain: \n{e:?}"
                );
            }
        }
    }

    #[test]
    fn test_call_intent_serialization() {
        // The call intent serialized value must match the expected enum names as
        // defined in the Element-Call repo:
        // https://github.com/element-hq/element-call/blob/de8fdcfa694659a29f2c7a4401dd09cfec846a96/src/UrlParams.ts#L32
        // The enum uses serde rename `snake_case` to serialize the values, but it makes
        // it invisible that it is important, so ensure that the values are
        // correct.
        assert_eq!(serde_json::to_string(&Intent::StartCall).unwrap(), r#""start_call""#);
        assert_eq!(serde_json::to_string(&Intent::JoinExisting).unwrap(), r#""join_existing""#);
        assert_eq!(
            serde_json::to_string(&Intent::JoinExistingDm).unwrap(),
            r#""join_existing_dm""#
        );
        assert_eq!(serde_json::to_string(&Intent::StartCallDm).unwrap(), r#""start_call_dm""#);
        assert_eq!(
            serde_json::to_string(&Intent::StartCallDmVoice).unwrap(),
            r#""start_call_dm_voice""#
        );
        assert_eq!(
            serde_json::to_string(&Intent::JoinExistingDmVoice).unwrap(),
            r#""join_existing_dm_voice""#
        );
    }
}
