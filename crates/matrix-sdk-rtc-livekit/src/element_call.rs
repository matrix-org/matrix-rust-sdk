use matrix_sdk::{
    Room as MatrixRoom,
    ruma::events::call::member::CallMemberStateKey,
    widget::{
        ClientProperties, ElementCallWidget, ElementCallWidgetOptions, EncryptionSystem, Intent,
        publish_call_membership_via_widget, send_hangup_via_widget, start_element_call_widget,
    },
};

use crate::{LiveKitError, LiveKitResult};

/// Resolve the Element Call encryption mode from the room state.
pub async fn element_call_encryption_for_room(
    room: &MatrixRoom,
) -> LiveKitResult<EncryptionSystem> {
    let encryption_state = room.latest_encryption_state().await.map_err(LiveKitError::widget)?;

    if encryption_state.is_encrypted() {
        return Ok(EncryptionSystem::PerParticipantKeys);
    }

    Ok(EncryptionSystem::Unencrypted)
}

/// Start Element Call with sensible defaults for a MatrixRTC join flow.
pub async fn start_element_call_widget_for_room(
    room: MatrixRoom,
    element_call_url: impl Into<String>,
) -> LiveKitResult<LiveKitElementCallWidget> {
    let options = ElementCallWidgetOptions {
        widget_id: "element-call".to_owned(),
        parent_url: None,
        encryption: element_call_encryption_for_room(&room).await?,
        intent: Intent::JoinExisting,
        client_properties: ClientProperties::new("matrix-sdk-rtc-livekit-join", None, None),
    };

    LiveKitElementCallWidget::start(room, element_call_url, options).await
}

/// Running Element Call widget integration for a LiveKit room session.
#[derive(Debug)]
pub struct LiveKitElementCallWidget {
    room: MatrixRoom,
    widget: ElementCallWidget,
    shutdown_membership_state_key: CallMemberStateKey,
}

impl LiveKitElementCallWidget {
    /// Start the Element Call widget and initialize metadata needed for hangup signaling.
    pub async fn start(
        room: MatrixRoom,
        element_call_url: impl Into<String>,
        options: ElementCallWidgetOptions,
    ) -> LiveKitResult<Self> {
        let widget = start_element_call_widget(room.clone(), element_call_url.into(), options)
            .await
            .map_err(LiveKitError::widget)?;

        let own_user_id = room
            .client()
            .user_id()
            .ok_or_else(|| {
                LiveKitError::widget(std::io::Error::other(
                    "missing user id for widget shutdown event",
                ))
            })?
            .to_owned();
        let own_device_id = room
            .client()
            .device_id()
            .ok_or_else(|| {
                LiveKitError::widget(std::io::Error::other(
                    "missing device id for widget shutdown event",
                ))
            })?
            .to_owned();

        let shutdown_membership_state_key =
            CallMemberStateKey::new(own_user_id, Some(own_device_id.to_string()), true);

        Ok(Self { room, widget, shutdown_membership_state_key })
    }

    /// Publish an active-call membership update via the widget API.
    pub async fn publish_membership(&self, service_url: &str) -> LiveKitResult<()> {
        publish_call_membership_via_widget(self.room.clone(), &self.widget, service_url)
            .await
            .map_err(LiveKitError::widget)
    }

    /// Send a shutdown membership update via the widget API.
    pub async fn send_hangup(&self) -> LiveKitResult<()> {
        send_hangup_via_widget(&self.widget, Some(&self.shutdown_membership_state_key))
            .await
            .map_err(LiveKitError::widget)
    }
}
