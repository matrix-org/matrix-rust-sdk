use std::sync::Arc;

use matrix_sdk::async_trait;

use crate::room::Room;

#[derive(uniffi::Record)]
pub struct Widget {
    /// Information about the widget.
    pub info: WidgetInfo,
    /// Communication channels with a widget.
    pub comm: Arc<WidgetComm>,
}

impl From<Widget> for matrix_sdk::widget::Widget {
    fn from(value: Widget) -> Self {
        let comm = &value.comm.0;
        Self {
            info: value.info.into(),
            comm: matrix_sdk::widget::Comm { from: comm.from.clone(), to: comm.to.clone() },
        }
    }
}

/// Information about a widget.
#[derive(uniffi::Record)]
pub struct WidgetInfo {
    /// Widget's unique identifier.
    pub id: String,
    /// Whether or not the widget should be initialized on load message
    /// (`ContentLoad` message), or upon creation/attaching of the widget to
    /// the SDK's state machine that drives the API.
    pub init_on_load: bool,
}

impl From<WidgetInfo> for matrix_sdk::widget::Info {
    fn from(value: WidgetInfo) -> Self {
        let WidgetInfo { id, init_on_load } = value;
        Self { id, init_on_load }
    }
}

/// Communication "pipes" with a widget.
#[derive(uniffi::Object)]
pub struct WidgetComm(matrix_sdk::widget::Comm);

/// Permissions that a widget can request from a client.
#[derive(uniffi::Record)]
pub struct WidgetPermissions {
    /// Types of the messages that a widget wants to be able to fetch.
    pub read: Vec<WidgetEventFilter>,
    /// Types of the messages that a widget wants to be able to send.
    pub send: Vec<WidgetEventFilter>,
}

impl From<WidgetPermissions> for matrix_sdk::widget::Permissions {
    fn from(value: WidgetPermissions) -> Self {
        Self {
            read: value.read.into_iter().map(Into::into).collect(),
            send: value.send.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<matrix_sdk::widget::Permissions> for WidgetPermissions {
    fn from(value: matrix_sdk::widget::Permissions) -> Self {
        Self {
            read: value.read.into_iter().map(Into::into).collect(),
            send: value.send.into_iter().map(Into::into).collect(),
        }
    }
}

/// Different kinds of filters that could be applied to the timeline events.
#[derive(uniffi::Enum)]
pub enum WidgetEventFilter {
    /// Message-like events.
    MessageLike {
        /// The type of the message-like event.
        event_type: String,
        /// Additional filter for the msgtype, currently only used for
        /// `m.room.message`.
        msgtype: Option<String>,
    },
    /// State events.
    State {
        /// The type of the state event.
        event_type: String,
        /// State key that could be `None`, `None` means "any state key".
        state_key: Option<String>,
    },
}

impl From<WidgetEventFilter> for matrix_sdk::widget::EventFilter {
    fn from(value: WidgetEventFilter) -> Self {
        match value {
            WidgetEventFilter::MessageLike { event_type, msgtype } => {
                Self::MessageLike { event_type: event_type.into(), msgtype }
            }
            WidgetEventFilter::State { event_type, state_key } => {
                Self::State { event_type: event_type.into(), state_key }
            }
        }
    }
}

impl From<matrix_sdk::widget::EventFilter> for WidgetEventFilter {
    fn from(value: matrix_sdk::widget::EventFilter) -> Self {
        use matrix_sdk::widget::EventFilter as F;

        match value {
            F::MessageLike { event_type, msgtype } => {
                Self::MessageLike { event_type: event_type.to_string(), msgtype }
            }
            F::State { event_type, state_key } => {
                Self::State { event_type: event_type.to_string(), state_key }
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
        tokio::task::spawn_blocking(move || this.acquire_permissions(permissions.into()).into())
            .await
            // propagate panics from the blocking task
            .unwrap()
    }
}

#[uniffi::export]
pub async fn run_widget_api(
    room: Arc<Room>,
    widget: Widget,
    permissions_provider: Box<dyn WidgetPermissionsProvider>,
) {
    let permissions_provider = PermissionsProviderWrap(permissions_provider.into());
    if let Err(()) =
        matrix_sdk::widget::run_widget_api(room.inner.clone(), widget.into(), permissions_provider)
            .await
    {}
}
