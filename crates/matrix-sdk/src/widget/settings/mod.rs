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

use language_tags::LanguageTag;
use ruma::{
    DeviceId, RoomId, UserId,
    api::client::profile::{AvatarUrl, DisplayName, get_profile},
};
use url::Url;

use crate::Room;

mod element_call;
mod url_params;

pub use self::element_call::{
    EncryptionSystem, Intent, VirtualElementCallWidgetConfig, VirtualElementCallWidgetProperties,
};

/// Settings of the widget.
#[derive(Debug, Clone)]
pub struct WidgetSettings {
    widget_id: String,
    init_on_content_load: bool,
    raw_url: Url,
}

impl WidgetSettings {
    /// Create a new WidgetSettings instance
    pub fn new(
        id: String,
        init_on_content_load: bool,
        raw_url: &str,
    ) -> Result<Self, url::ParseError> {
        Ok(Self { widget_id: id, init_on_content_load, raw_url: Url::parse(raw_url)? })
    }

    /// Widget's unique identifier.
    pub fn widget_id(&self) -> &str {
        &self.widget_id
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
    /// * `room` - A Matrix room which is used to query the logged in username
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
            room.client().account().fetch_user_profile().await.unwrap_or_default(),
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
        let avatar_url = profile
            .get_static::<AvatarUrl>()
            .ok()
            .flatten()
            .map(|url| url.to_string())
            .unwrap_or_default();

        let query_props = url_params::QueryProperties {
            widget_id: self.widget_id.clone(),
            avatar_url,
            display_name: profile.get_static::<DisplayName>().ok().flatten().unwrap_or_default(),
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
