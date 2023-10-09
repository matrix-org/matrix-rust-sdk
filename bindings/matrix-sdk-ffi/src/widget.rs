use std::sync::{Arc, Mutex};

use language_tags::LanguageTag;
use matrix_sdk::{
    async_trait,
    widget::{MessageLikeEventFilter, StateEventFilter},
};
use tracing::error;

use crate::{room::Room, RUNTIME};

#[derive(uniffi::Record)]
pub struct WidgetDriverAndHandle {
    pub driver: Arc<WidgetDriver>,
    pub handle: Arc<WidgetDriverHandle>,
}

#[uniffi::export]
pub fn make_widget_driver(settings: WidgetSettings) -> Result<WidgetDriverAndHandle, ParseError> {
    let (driver, handle) = matrix_sdk::widget::WidgetDriver::new(settings.try_into()?);
    Ok(WidgetDriverAndHandle {
        driver: Arc::new(WidgetDriver(Mutex::new(Some(driver)))),
        handle: Arc::new(WidgetDriverHandle(handle)),
    })
}

/// An object that handles all interactions of a widget living inside a webview
/// or IFrame with the Matrix world.
#[derive(uniffi::Object)]
pub struct WidgetDriver(Mutex<Option<matrix_sdk::widget::WidgetDriver>>);

#[uniffi::export(async_runtime = "tokio")]
impl WidgetDriver {
    pub async fn run(
        &self,
        room: Arc<Room>,
        permissions_provider: Box<dyn WidgetPermissionsProvider>,
    ) {
        let Some(driver) = self.0.lock().unwrap().take() else {
            error!("Can't call run multiple times on a WidgetDriver");
            return;
        };

        let permissions_provider = PermissionsProviderWrap(permissions_provider.into());
        if let Err(()) = driver.run(room.inner.clone(), permissions_provider).await {
            // TODO
        }
    }
}

/// Information about a widget.
#[derive(uniffi::Record, Clone)]
pub struct WidgetSettings {
    /// Widget's unique identifier.
    pub id: String,
    /// Whether or not the widget should be initialized on load message
    /// (`ContentLoad` message), or upon creation/attaching of the widget to
    /// the SDK's state machine that drives the API.
    pub init_after_content_load: bool,
    /// This contains the url from the widget state event.
    /// In this url placeholders can be used to pass information from the client
    /// to the widget. Possible values are: `$widgetId`, `$parentUrl`,
    /// `$userId`, `$lang`, `$fontScale`, `$analyticsID`.
    ///
    /// # Examples
    ///
    /// e.g `http://widget.domain?username=$userId`
    /// will become: `http://widget.domain?username=@user_matrix_id:server.domain`.
    raw_url: String,
}

impl TryFrom<WidgetSettings> for matrix_sdk::widget::WidgetSettings {
    type Error = ParseError;

    fn try_from(value: WidgetSettings) -> Result<Self, Self::Error> {
        let WidgetSettings { id, init_after_content_load, raw_url } = value;
        Ok(matrix_sdk::widget::WidgetSettings::new(id, init_after_content_load, &raw_url)?)
    }
}

impl From<matrix_sdk::widget::WidgetSettings> for WidgetSettings {
    fn from(value: matrix_sdk::widget::WidgetSettings) -> Self {
        let (id, init_after_content_load, raw_url) =
            (value.id().to_owned(), value.init_on_content_load(), value.raw_url().to_string());
        WidgetSettings { id, init_after_content_load, raw_url }
    }
}

/// Create the actual url that can be used to setup the WebView or IFrame
/// that contains the widget.
///
/// # Arguments
/// * `widget_settings` - The widget settings to generate the url for.
/// * `room` - A matrix room which is used to query the logged in username
/// * `props` - Properties from the client that can be used by a widget to adapt
///   to the client. e.g. language, font-scale...
#[uniffi::export(async_runtime = "tokio")]
pub async fn generate_webview_url(
    widget_settings: WidgetSettings,
    room: Arc<Room>,
    props: ClientProperties,
) -> Result<String, ParseError> {
    Ok(matrix_sdk::widget::WidgetSettings::generate_webview_url(
        &widget_settings.clone().try_into()?,
        &room.inner,
        props.into(),
    )
    .await
    .map(|url| url.to_string())?)
}

/// Properties to create a new virtual Element Call widget.
#[derive(uniffi::Record, Clone)]
pub struct VirtualElementCallWidgetOptions {
    /// the url to the app e.g. <https://call.element.io>, <https://call.element.dev>
    pub element_call_url: String,
    /// the widget id.
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
    /// defines if the branding header of Element call should be hidden.
    /// (default: `true`)
    pub hide_header: Option<bool>,
    /// if set, the lobby will be skipped and the widget will join the
    /// call on the `io.element.join` action. (default: `false`)
    pub preload: Option<bool>,
    /// The font scale which will be used inside element call. (default: `1`)
    pub font_scale: Option<f64>,
    /// whether element call should prompt the user to open in the browser or
    /// the app (default: `false`).
    pub app_prompt: Option<bool>,
    ///Don't show the lobby and join the call immediately. (default: `false`)
    pub skip_lobby: Option<bool>,
    ///Make it not possible to get to the calls list in the webview. (default:
    /// `true`)
    pub confine_to_room: Option<bool>,
    /// A list of fonts to adapt to ios/android system fonts. (default:`[]`)
    pub fonts: Option<Vec<String>>,
    /// Can be used to pass a PostHog id to element call.
    pub analytics_id: Option<String>,
}

impl From<VirtualElementCallWidgetOptions> for matrix_sdk::widget::VirtualElementCallWidgetOptions {
    fn from(value: VirtualElementCallWidgetOptions) -> Self {
        Self {
            element_call_url: value.element_call_url,
            widget_id: value.widget_id,
            parent_url: value.parent_url,
            hide_header: value.hide_header,
            preload: value.preload,
            font_scale: value.font_scale,
            app_prompt: value.app_prompt,
            skip_lobby: value.skip_lobby,
            confine_to_room: value.confine_to_room,
            fonts: value.fonts,
            analytics_id: value.analytics_id,
        }
    }
}

/// `WidgetSettings` are usually created from a state event.
/// (currently unimplemented)
///
/// In some cases the client wants to create custom `WidgetSettings`
/// for specific rooms based on other conditions.
/// This function returns a `WidgetSettings` object which can be used
/// to setup a widget using `run_client_widget_api`
/// and to generate the correct url for the widget.
///  # Arguments
/// * - `props` A struct containing the configuration parameters for a element
///   call widget.
#[uniffi::export]
pub fn new_virtual_element_call_widget(
    props: VirtualElementCallWidgetOptions,
) -> Result<WidgetSettings, ParseError> {
    Ok(matrix_sdk::widget::WidgetSettings::new_virtual_element_call_widget(props.into())
        .map(|w| w.into())?)
}

#[derive(uniffi::Record)]
pub struct ClientProperties {
    /// The client_id provides the widget with the option to behave differently
    /// for different clients. e.g org.example.ios.
    client_id: String,
    /// The language tag the client is set to e.g. en-us. (Undefined and invalid
    /// becomes: `en-US`)
    language_tag: Option<String>,
    /// A string describing the theme (dark, light) or org.example.dark.
    /// (default: `light`)
    theme: Option<String>,
}

impl From<ClientProperties> for matrix_sdk::widget::ClientProperties {
    fn from(value: ClientProperties) -> Self {
        let ClientProperties { client_id, language_tag, theme } = value;
        let language_tag = language_tag.and_then(|l| LanguageTag::parse(&l).ok());
        Self::new(&client_id, language_tag, theme)
    }
}

/// A handle that encapsulates the communication between a widget driver and the
/// corresponding widget (inside a webview or IFrame).
#[derive(uniffi::Object)]
pub struct WidgetDriverHandle(matrix_sdk::widget::WidgetDriverHandle);

#[uniffi::export(async_runtime = "tokio")]
impl WidgetDriverHandle {
    /// Receive a message from the widget driver.
    ///
    /// The message must be passed on to the widget.
    ///
    /// Returns `None` if the widget driver is no longer running.
    pub async fn recv(&self) -> Option<String> {
        self.0.recv().await
    }

    //// Send a message from the widget to the widget driver.
    ///
    /// Returns `false` if the widget driver is no longer running.
    pub async fn send(&self, msg: String) -> bool {
        self.0.send(msg).await
    }
}

/// Permissions that a widget can request from a client.
#[derive(uniffi::Record)]
pub struct WidgetPermissions {
    /// Types of the messages that a widget wants to be able to fetch.
    pub read: Vec<WidgetEventFilter>,
    /// Types of the messages that a widget wants to be able to send.
    pub send: Vec<WidgetEventFilter>,
    /// If this permission is requested by the widget, it can not operate
    /// separately from the matrix client.
    ///
    /// This means clients should not offer to open the widget in a separate
    /// browser/tab/webview that is not connected to the postmessage widget-api.
    pub requires_client: bool,
}

impl From<WidgetPermissions> for matrix_sdk::widget::Permissions {
    fn from(value: WidgetPermissions) -> Self {
        Self {
            read: value.read.into_iter().map(Into::into).collect(),
            send: value.send.into_iter().map(Into::into).collect(),
            requires_client: value.requires_client,
        }
    }
}

impl From<matrix_sdk::widget::Permissions> for WidgetPermissions {
    fn from(value: matrix_sdk::widget::Permissions) -> Self {
        Self {
            read: value.read.into_iter().map(Into::into).collect(),
            send: value.send.into_iter().map(Into::into).collect(),
            requires_client: value.requires_client,
        }
    }
}

/// Different kinds of filters that could be applied to the timeline events.
#[derive(uniffi::Enum)]
pub enum WidgetEventFilter {
    /// Matches message-like events with the given `type`.
    MessageLikeWithType { event_type: String },
    /// Matches `m.room.message` events with the given `msgtype`.
    RoomMessageWithMsgtype { msgtype: String },
    /// Matches state events with the given `type`, regardless of `state_key`.
    StateWithType { event_type: String },
    /// Matches state events with the given `type` and `state_key`.
    StateWithTypeAndStateKey { event_type: String, state_key: String },
}

impl From<WidgetEventFilter> for matrix_sdk::widget::EventFilter {
    fn from(value: WidgetEventFilter) -> Self {
        match value {
            WidgetEventFilter::MessageLikeWithType { event_type } => {
                Self::MessageLike(MessageLikeEventFilter::WithType(event_type.into()))
            }
            WidgetEventFilter::RoomMessageWithMsgtype { msgtype } => {
                Self::MessageLike(MessageLikeEventFilter::RoomMessageWithMsgtype(msgtype))
            }
            WidgetEventFilter::StateWithType { event_type } => {
                Self::State(StateEventFilter::WithType(event_type.into()))
            }
            WidgetEventFilter::StateWithTypeAndStateKey { event_type, state_key } => {
                Self::State(StateEventFilter::WithTypeAndStateKey(event_type.into(), state_key))
            }
        }
    }
}

impl From<matrix_sdk::widget::EventFilter> for WidgetEventFilter {
    fn from(value: matrix_sdk::widget::EventFilter) -> Self {
        use matrix_sdk::widget::EventFilter as F;

        match value {
            F::MessageLike(MessageLikeEventFilter::WithType(event_type)) => {
                Self::MessageLikeWithType { event_type: event_type.to_string() }
            }
            F::MessageLike(MessageLikeEventFilter::RoomMessageWithMsgtype(msgtype)) => {
                Self::RoomMessageWithMsgtype { msgtype }
            }
            F::State(StateEventFilter::WithType(event_type)) => {
                Self::StateWithType { event_type: event_type.to_string() }
            }
            F::State(StateEventFilter::WithTypeAndStateKey(event_type, state_key)) => {
                Self::StateWithTypeAndStateKey { event_type: event_type.to_string(), state_key }
            }
        }
    }
}

#[uniffi::export(callback_interface)]
pub trait WidgetPermissionsProvider: Send + Sync {
    fn acquire_permissions(&self, permissions: WidgetPermissions) -> WidgetPermissions;
}

struct PermissionsProviderWrap(Arc<dyn WidgetPermissionsProvider>);

#[async_trait]
impl matrix_sdk::widget::PermissionsProvider for PermissionsProviderWrap {
    async fn acquire_permissions(
        &self,
        permissions: matrix_sdk::widget::Permissions,
    ) -> matrix_sdk::widget::Permissions {
        let this = self.0.clone();
        // This could require a prompt to the user. Ideally the callback
        // interface would just be async, but that's not supported yet so use
        // one of tokio's blocking task threads instead.
        RUNTIME
            .spawn_blocking(move || this.acquire_permissions(permissions.into()).into())
            .await
            // propagate panics from the blocking task
            .unwrap()
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum ParseError {
    #[error("empty host")]
    EmptyHost,
    #[error("invalid international domain name")]
    IdnaError,
    #[error("invalid port number")]
    InvalidPort,
    #[error("invalid IPv4 address")]
    InvalidIpv4Address,
    #[error("invalid IPv6 address")]
    InvalidIpv6Address,
    #[error("invalid domain character")]
    InvalidDomainCharacter,
    #[error("relative URL without a base")]
    RelativeUrlWithoutBase,
    #[error("relative URL with a cannot-be-a-base base")]
    RelativeUrlWithCannotBeABaseBase,
    #[error("a cannot-be-a-base URL doesn’t have a host to set")]
    SetHostOnCannotBeABaseUrl,
    #[error("URLs more than 4 GB are not supported")]
    Overflow,
    #[error("unknown URL parsing error")]
    Other,
}

impl From<url::ParseError> for ParseError {
    fn from(value: url::ParseError) -> Self {
        match value {
            url::ParseError::EmptyHost => Self::EmptyHost,
            url::ParseError::IdnaError => Self::IdnaError,
            url::ParseError::InvalidPort => Self::InvalidPort,
            url::ParseError::InvalidIpv4Address => Self::InvalidIpv4Address,
            url::ParseError::InvalidIpv6Address => Self::InvalidIpv6Address,
            url::ParseError::InvalidDomainCharacter => Self::InvalidDomainCharacter,
            url::ParseError::RelativeUrlWithoutBase => Self::RelativeUrlWithoutBase,
            url::ParseError::RelativeUrlWithCannotBeABaseBase => {
                Self::RelativeUrlWithCannotBeABaseBase
            }
            url::ParseError::SetHostOnCannotBeABaseUrl => Self::SetHostOnCannotBeABaseUrl,
            url::ParseError::Overflow => Self::Overflow,
            _ => Self::Other,
        }
    }
}
