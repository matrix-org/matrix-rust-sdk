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

//! All Element Call related code.

use serde::Serialize;
use url::Url;

use super::{url_params, WidgetSettings};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ElementCallParams {
    user_id: String,
    room_id: String,
    widget_id: String,
    avatar_url: String,
    displayname: String,
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
    fonts: Option<String>,
}

/// Properties to create a new virtual Element Call widget.
#[derive(Debug)]
pub struct VirtualElementCallWidgetOptions {
    /// The url of the app e.g. <https://call.element.io>, <https://call.element.dev>.
    pub element_call_url: String,
    /// The widget id.
    pub widget_id: String,
    /// The url that is used as the target for the PostMessages sent
    /// by the widget (to the client). For a web app client this is the client
    /// url. In case of using other platforms the client most likely is setup
    /// up to listen to postmessages in the same webview the widget is
    /// hosted. In this case the parent_url is set to the url of the
    /// webview with the widget. Be aware, that this means, the widget
    /// will receive its own postmessage messages. The matrix-widget-api
    /// (js) ignores those so this works but it might break custom
    /// implementations. So always keep this in mind. Defaults to
    /// `element_call_url` for the non IFrame (dedicated webview) usecase.
    pub parent_url: Option<String>,
    /// Defines if the branding header of Element call should be hidden.
    /// (default: `true`)
    pub hide_header: Option<bool>,
    /// If set, the lobby will be skipped and the widget will join the
    /// call on the `io.element.join` action. (default: `false`)
    pub preload: Option<bool>,
    /// The font scale which will be used inside element call. (default: `1`)
    pub font_scale: Option<f64>,
    /// Whether element call should prompt the user to open in the browser or
    /// the app. (default: `false`)
    pub app_prompt: Option<bool>,
    /// Don't show the lobby and join the call immediately. (default: `false`)
    pub skip_lobby: Option<bool>,
    /// Make it not possible to get to the calls list in the webview. (default:
    /// `true`)
    pub confine_to_room: Option<bool>,
    /// A list of fonts to adapt to ios/android system fonts. (default:`[]`)
    pub fonts: Option<Vec<String>>,
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
            avatar_url: url_params::AVATAR_URL.to_owned(),
            displayname: url_params::DISPLAY_NAME.to_owned(),
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
            fonts: props.fonts.map(|fs| fs.join(",")),
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
