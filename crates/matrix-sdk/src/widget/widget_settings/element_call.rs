use serde::Serialize;
use url::Url;

use super::{url_params, WidgetSettings};

// All element call related code is separated into this file.
// The rest of the code is usable for generic widgets as well.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ElementCallParams {
    pub(crate) user_id: String,
    pub(crate) room_id: String,
    pub(crate) widget_id: String,
    pub(crate) avatar_url: String,
    pub(crate) displayname: String,
    pub(crate) lang: String,
    pub(crate) theme: String,
    pub(crate) client_id: String,
    pub(crate) device_id: String,
    pub(crate) base_url: String,
    // Non template parameters
    pub(crate) parent_url: String,
    pub(crate) skip_lobby: bool,
    pub(crate) confine_to_room: bool,
    pub(crate) app_prompt: bool,
    pub(crate) hide_header: bool,
    pub(crate) preload: bool,
    pub(crate) analytics_id: Option<String>,
    pub(crate) font_scale: Option<f64>,
    pub(crate) fonts: Option<String>,
}

impl WidgetSettings {
    /// `WidgetSettings` are usually created from a state event.
    /// (currently unimplemented)
    /// But in some cases the client wants to create custom `WidgetSettings`
    /// for specific rooms based on other conditions.
    /// This function returns a `WidgetSettings` object which can be used
    /// to setup a widget using `run_client_widget_api`
    /// and to generate the correct url for the widget.
    ///
    /// # Arguments
    /// * `element_call_url` - the url to the app e.g. <https://call.element.io>,
    ///   <https://call.element.dev>
    /// * `id` - the widget id.
    /// * `parentUrl` - The url that is used as the target for the PostMessages
    ///   sent by the widget (to the client). For a web app client this is the
    ///   client url. In case of using other platforms the client most likely is
    ///   setup up to listen to postmessages in the same webview the widget is
    ///   hosted. In this case the parent_url is set to the url of the webview
    ///   with the widget. Be aware, that this means, the widget will receive
    ///   its own postmessage messages. The matrix-widget-api (js) ignores those
    ///   so this works but it might break custom implementations. So always
    ///   keep this in mind. Defaults to `element_call_url` for the non IFrame
    ///   (dedicated webview) usecase.
    /// * `hide_header` - defines if the branding header of Element call should
    ///   be hidden. (default: `true`)
    /// * `preload` - if set, the lobby will be skipped and the widget will join
    ///   the call on the `io.element.join` action. (default: `false`)
    /// * `font_scale` - The font scale which will be used inside element call.
    ///   (default: `1`)
    /// * `app_prompt` - whether element call should prompt the user to open in
    ///   the browser or the app (default: `false`).
    /// * `skip_lobby` Don't show the lobby and join the call immediately.
    ///   (default: `false`)
    /// * `confine_to_room` Make it not possible to get to the calls list in the
    ///   webview. (default: `true`)
    /// * `fonts` A list of fonts to adapt to ios/android system fonts.
    ///   (default: `[]`)
    /// * `analytics_id` - Can be used to pass a PostHog id to element call.
    pub fn new_virtual_element_call_widget(
        element_call_url: String,
        widget_id: String,
        parent_url: Option<String>,
        hide_header: Option<bool>,
        preload: Option<bool>,
        font_scale: Option<f64>,
        app_prompt: Option<bool>,
        skip_lobby: Option<bool>,
        confine_to_room: Option<bool>,
        fonts: Option<Vec<String>>,
        analytics_id: Option<String>,
    ) -> Result<Self, url::ParseError> {
        let mut raw_url: Url = Url::parse(&format!("{element_call_url}/room"))?;

        let query_params = ElementCallParams {
            user_id: url_params::USER_ID.to_string(),
            room_id: url_params::ROOM_ID.to_string(),
            widget_id: url_params::WIDGET_ID.to_string(),
            avatar_url: url_params::AVATAR_URL.to_string(),
            displayname: url_params::DISPLAY_NAME.to_string(),
            lang: url_params::LANGUAGE.to_string(),
            theme: url_params::CLIENT_THEME.to_string(),
            client_id: url_params::CLIENT_ID.to_string(),
            device_id: url_params::DEVICE_ID.to_string(),
            base_url: url_params::HOMESERVER_URL.to_string(),

            parent_url: parent_url.unwrap_or(element_call_url.clone()),
            skip_lobby: skip_lobby.unwrap_or(false),
            confine_to_room: confine_to_room.unwrap_or(true),
            app_prompt: app_prompt.unwrap_or(false),
            hide_header: hide_header.unwrap_or(true),
            preload: preload.unwrap_or(false),
            analytics_id,
            font_scale,
            fonts: fonts.map(|fs| fs.join(",")),
        };

        let query =
            serde_urlencoded::to_string(query_params).map_err(|_| url::ParseError::Overflow)?;

        // Revert the encoding for the template parameters. So we can have a unified
        // replace logic.
        let query = query.replace("%24", "$");

        // All the params will be set inside the fragment (to keep the traffic to the
        // server minimal and most importantly don't send the passwords)
        raw_url.set_fragment(Some(&format!("?{}", query)));

        // for EC we always want init on content load to be true.
        Ok(Self { id: widget_id, init_after_content_load: true, raw_url })
    }
}
