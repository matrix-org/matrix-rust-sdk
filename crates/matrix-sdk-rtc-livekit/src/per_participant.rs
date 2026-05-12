//! Per-participant E2EE helpers for LiveKit + MatrixRTC.

use std::{sync::Arc, time::Duration};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
};
use livekit::{
    RoomEvent,
    e2ee::{
        E2eeOptions, EncryptionType,
        key_provider::{KeyDerivationFunction, KeyProvider, KeyProviderOptions},
    },
    id::ParticipantIdentity,
};
use matrix_sdk::{Client, Room, RoomMemberships, event_handler::EventHandlerDropGuard};
use matrix_sdk_base::crypto::CollectStrategy;
use rand::{RngCore, rngs::OsRng};
use ruma::serde::Raw;
use ruma::{OwnedRoomId, events::AnyToDeviceEvent};
use serde::Deserialize;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::LiveKitRoomOptionsProvider;

/// Runtime context for per-participant LiveKit E2EE.
#[derive(Clone)]
pub struct PerParticipantE2eeContext {
    pub key_provider: Arc<KeyProvider>,
    pub key_index: i32,
    pub local_key: Vec<u8>,
}

/// LiveKit room options provider with optional per-participant E2EE.
#[derive(Clone)]
pub struct E2eeRoomOptionsProvider {
    pub e2ee: Option<PerParticipantE2eeContext>,
}

impl LiveKitRoomOptionsProvider for E2eeRoomOptionsProvider {
    fn room_options(&self) -> livekit::RoomOptions {
        let mut options = livekit::RoomOptions::default();
        if let Some(context) = &self.e2ee {
            options.encryption = Some(E2eeOptions {
                encryption_type: EncryptionType::Gcm,
                key_provider: KeyProvider::clone(context.key_provider.as_ref()),
            });
        }
        options
    }
}

/// Precomputed per-participant E2EE providers used by the example runtime.
pub struct PerParticipantE2eeProviders {
    pub room_options_provider: E2eeRoomOptionsProvider,
    pub to_device_key_provider: Arc<KeyProvider>,
}

/// Build the to-device key provider and room options provider from an optional E2EE context.
pub fn build_per_participant_providers(
    room: &Room,
    e2ee: Option<PerParticipantE2eeContext>,
) -> PerParticipantE2eeProviders {
    let to_device_key_provider = if let Some(context) = e2ee.as_ref() {
        Arc::clone(&context.key_provider)
    } else {
        warn!(
            room_id = %room.room_id(),
            "per-participant E2EE context unavailable; registering to-device handler with fallback key provider"
        );
        let mut fallback_options = KeyProviderOptions::default();
        fallback_options.ratchet_window_size = 10;
        fallback_options.key_ring_size = 256;
        fallback_options.key_derivation_function = KeyDerivationFunction::HKDF;
        Arc::new(KeyProvider::new(fallback_options))
    };

    let room_options_provider = E2eeRoomOptionsProvider { e2ee };

    PerParticipantE2eeProviders { room_options_provider, to_device_key_provider }
}

/// Seed the local participant key into the key provider when user/device IDs are available.
pub fn seed_local_participant_key(client: &Client, e2ee: Option<&PerParticipantE2eeContext>) {
    if let Some(context) = e2ee
        && let (Some(user_id), Some(device_id)) = (client.user_id(), client.device_id())
    {
        let identity = ParticipantIdentity(format!("{user_id}:{device_id}"));
        let key_set =
            context.key_provider.set_key(&identity, context.key_index, context.local_key.clone());
        info!(
            %identity,
            key_index = context.key_index,
            key_set,
            "seeded local per-participant E2EE key_provider key before LiveKit connect"
        );
    }
}

/// Runtime wiring needed by callers after preparing per-participant E2EE.
pub struct PreparedPerParticipantE2ee {
    pub context: Option<PerParticipantE2eeContext>,
    pub room_options_provider: E2eeRoomOptionsProvider,
    pub to_device_guard: EventHandlerDropGuard,
}

/// Periodically resend local per-participant keys.
pub fn spawn_periodic_e2ee_key_resend(
    room: Room,
    context: PerParticipantE2eeContext,
    interval_secs: u64,
) {
    if interval_secs == 0 {
        return;
    }

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        loop {
            interval.tick().await;
            info!(
                interval_secs,
                key_index = context.key_index,
                "periodic per-participant E2EE key resend"
            );
            if let Err(err) =
                send_per_participant_keys(&room, context.key_index, &context.local_key, None).await
            {
                info!(?err, "failed to resend per-participant E2EE keys");
            }
        }
    });
}

/// Build, seed, and wire all per-participant E2EE runtime helpers for a room.
pub async fn prepare_per_participant_e2ee(
    room: &Room,
    periodic_resend_secs: u64,
) -> Result<PreparedPerParticipantE2ee, PerParticipantE2eeError> {
    let context =
        build_per_participant_e2ee(room).await?;
    seed_local_participant_key(&room.client(), context.as_ref());

    let providers = build_per_participant_providers(room, context.clone());
    let to_device_guard = register_e2ee_to_device_handler(
        &room.client(),
        room.room_id().to_owned(),
        providers.to_device_key_provider,
    );

    if let Some(context) = context.as_ref() {
        spawn_periodic_e2ee_key_resend(room.clone(), context.clone(), periodic_resend_secs);
    }

    Ok(PreparedPerParticipantE2ee {
        context,
        room_options_provider: providers.room_options_provider,
        to_device_guard,
    })
}

#[derive(Debug, Error)]
pub enum PerParticipantE2eeError {
    #[error("missing device id for per-participant E2EE")]
    MissingDeviceId,
    #[error("missing user id for per-participant E2EE")]
    MissingUserId,
    #[error(transparent)]
    Matrix(#[from] matrix_sdk::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

/// Build the initial per-participant E2EE context for a Matrix room.
pub async fn build_per_participant_e2ee(
    room: &Room,
) -> Result<Option<PerParticipantE2eeContext>, PerParticipantE2eeError> {
    let mut key_provider_options = KeyProviderOptions::default();
    key_provider_options.ratchet_window_size = 10;
    key_provider_options.key_ring_size = 256;
    key_provider_options.key_derivation_function = KeyDerivationFunction::HKDF;

    let key_provider = Arc::new(KeyProvider::new(key_provider_options));
    let local_key = derive_per_participant_key();
    send_per_participant_keys(room, 0, &local_key, None).await?;

    Ok(Some(PerParticipantE2eeContext { key_provider, key_index: 0, local_key }))
}

/// Generate local key material for per-participant media E2EE.
pub fn derive_per_participant_key() -> Vec<u8> {
    let mut key = [0u8; 16];
    OsRng.fill_bytes(&mut key);
    key.to_vec()
}

/// Send `io.element.call.encryption_keys` to room members' devices.
pub async fn send_per_participant_keys(
    room: &Room,
    key_index: i32,
    key: &[u8],
    target_device_id: Option<&str>,
) -> Result<(), PerParticipantE2eeError> {
    if key.is_empty() {
        return Ok(());
    }

    let key = if key.len() >= 16 { &key[..16] } else { key };
    let client = room.client();
    let own_device_id =
        client.device_id().ok_or(PerParticipantE2eeError::MissingDeviceId)?.to_owned();
    let own_user_id = client.user_id().map(|id| id.to_owned());

    let members = room.members(RoomMemberships::JOIN).await?;
    let mut recipients = Vec::new();
    for member in members {
        let devices = client.encryption().get_user_devices(member.user_id()).await?;
        for device in devices.devices() {
            if let Some(own_user_id) = own_user_id.as_ref()
                && device.user_id() == own_user_id
                && device.device_id() == &own_device_id
            {
                continue;
            }
            if target_device_id.is_none_or(|target| device.device_id().as_str() == target) {
                recipients.push(device);
            }
        }
    }

    if recipients.is_empty() {
        return Ok(());
    }

    let key_b64 = URL_SAFE_NO_PAD.encode(key);
    let sent_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let own_user_id = client.user_id().ok_or(PerParticipantE2eeError::MissingUserId)?;
    let claimed = own_device_id.as_str();
    let member_id = format!("{own_user_id}:{claimed}");

    let content_raw = Raw::new(&serde_json::json!({
        "keys": { "index": key_index, "key": key_b64 },
        "device_id": claimed,
        "member": { "claimed_device_id": claimed, "id": member_id },
        "room_id": room.room_id().to_string(),
        "session": {
            "application": "m.call",
            "call_id": "",
            "scope": "m.room"
        },
        "sent_ts": sent_ts,
    }))?
    .cast_unchecked();

    let _ = client
        .encryption()
        .encrypt_and_send_raw_to_device(
            recipients.iter().collect(),
            "io.element.call.encryption_keys",
            content_raw,
            CollectStrategy::AllDevices,
        )
        .await?;

    Ok(())
}

/// Apply per-participant E2EE setup immediately after joining a LiveKit room.
pub async fn handle_per_participant_joined(
    room: &Room,
    room_handle: &Arc<livekit::Room>,
    events: Option<crate::LiveKitRoomEvents>,
    context: Option<&PerParticipantE2eeContext>,
    key_grace_period_var: &str,
    key_grace_period_default_ms: u64,
) {
    let Some(context) = context else {
        return;
    };

    let identity = room_handle.local_participant().identity();
    let key_set =
        context.key_provider.set_key(&identity, context.key_index, context.local_key.clone());
    room_handle.e2ee_manager().set_enabled(true);
    info!(
        %identity,
        key_index = context.key_index,
        key_set,
        "enabled per-participant E2EE for local participant"
    );

    if let Err(err) =
        send_per_participant_keys(room, context.key_index, &context.local_key, None).await
    {
        info!(?err, "failed to send per-participant E2EE keys immediately after room connect");
    }

    let key_grace_period = per_participant_key_grace_period_from_env(
        key_grace_period_var,
        key_grace_period_default_ms,
    );
    if !key_grace_period.is_zero() {
        info!(
            key_grace_period_ms = key_grace_period.as_millis(),
            "waiting for per-participant E2EE key grace period before publishing media"
        );
        tokio::time::sleep(key_grace_period).await;
    }

    if let Some(events) = events {
        spawn_livekit_e2ee_event_resend(room.clone(), events, context.clone());
    }
}

/// Register a to-device handler that applies received per-participant keys.
pub fn register_e2ee_to_device_handler(
    client: &Client,
    room_id: OwnedRoomId,
    key_provider: Arc<KeyProvider>,
) -> EventHandlerDropGuard {
    let handle = client.add_event_handler(move |raw: Raw<AnyToDeviceEvent>| {
        let key_provider = Arc::clone(&key_provider);
        let room_id = room_id.clone();
        async move {
            let observed_event_type = raw
                .get_field::<String>("type")
                .ok()
                .flatten()
                .unwrap_or_else(|| "<missing>".to_owned());

            let event_map = match raw.deserialize_as::<serde_json::Map<String, serde_json::Value>>()
            {
                Ok(event_map) => event_map,
                Err(err) => {
                    warn!(
                        event_type = %observed_event_type,
                        ?err,
                        "failed to deserialize raw to-device event into JSON map"
                    );
                    return;
                }
            };
            let event_value = serde_json::Value::Object(event_map);
            let event =
                match serde_json::from_value::<IncomingKeyToDeviceEvent>(event_value.clone()) {
                    Ok(event) => event,
                    Err(err) => {
                        warn!(
                            event_type = %observed_event_type,
                            ?err,
                            raw_event = ?event_value,
                            "failed to parse to-device event as IncomingKeyToDeviceEvent"
                        );
                        return;
                    }
                };

            if event.event_type != "io.element.call.encryption_keys" {
                return;
            }

            let sender_device_id = event
                .content
                .device_id
                .as_deref()
                .or_else(|| {
                    event
                        .content
                        .member
                        .as_ref()
                        .and_then(|member| member.claimed_device_id.as_deref())
                })
                .or_else(|| {
                    event
                        .sender_device_keys
                        .as_ref()
                        .and_then(|sender_device_keys| sender_device_keys.device_id.as_deref())
                });

            let Some(sender_device_id) = sender_device_id else {
                warn!(
                    sender = %event.sender,
                    event_room_id = %event.content.room_id,
                    expected_room_id = %room_id,
                    "ignoring encryption key event without a sender device id"
                );
                return;
            };

            debug!(
                sender = %event.sender,
                sender_device = %sender_device_id,
                event_room_id = %event.content.room_id,
                expected_room_id = %room_id,
                key_count = event.content.keys.len(),
                "received io.element.call.encryption_keys to-device event"
            );
            if event.content.room_id != room_id.as_str() {
                warn!(
                    sender = %event.sender,
                    sender_device = %sender_device_id,
                    event_room_id = %event.content.room_id,
                    expected_room_id = %room_id,
                    "ignoring encryption key event for different room"
                );
                return;
            }

            let identity = ParticipantIdentity(format!("{}:{}", event.sender, sender_device_id));
            for key_entry in event.content.keys {
                let key_bytes = STANDARD_NO_PAD
                    .decode(&key_entry.key)
                    .or_else(|_| STANDARD.decode(&key_entry.key))
                    .or_else(|_| URL_SAFE_NO_PAD.decode(&key_entry.key))
                    .or_else(|_| URL_SAFE.decode(&key_entry.key));
                let Ok(key_bytes) = key_bytes else {
                    warn!(
                        %identity,
                        key_index = key_entry.index,
                        "failed to decode per-participant E2EE key"
                    );
                    continue;
                };
                let key_set = key_provider.set_key(&identity, key_entry.index, key_bytes);
                info!(
                    %identity,
                    key_index = key_entry.index,
                    key_set,
                    "installed per-participant E2EE key from to-device event"
                );
            }
        }
    });

    client.event_handler_drop_guard(handle)
}

#[derive(Deserialize)]
struct IncomingKeyToDeviceEvent {
    #[serde(rename = "type")]
    event_type: String,
    sender: String,
    content: IncomingKeyToDeviceContent,
    sender_device_keys: Option<IncomingSenderDeviceKeys>,
}

#[derive(Deserialize)]
struct IncomingKeyToDeviceContent {
    room_id: String,
    device_id: Option<String>,
    member: Option<IncomingKeyMember>,
    #[serde(deserialize_with = "deserialize_key_entries")]
    keys: Vec<IncomingKeyEntry>,
}

#[derive(Deserialize)]
struct IncomingKeyMember {
    claimed_device_id: Option<String>,
}

#[derive(Deserialize)]
struct IncomingSenderDeviceKeys {
    device_id: Option<String>,
}

#[derive(Deserialize)]
struct IncomingKeyEntry {
    index: i32,
    key: String,
}

fn deserialize_key_entries<'de, D>(deserializer: D) -> Result<Vec<IncomingKeyEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Keys {
        One(IncomingKeyEntry),
        Many(Vec<IncomingKeyEntry>),
    }

    match Keys::deserialize(deserializer)? {
        Keys::One(entry) => Ok(vec![entry]),
        Keys::Many(entries) => Ok(entries),
    }
}

/// Resend keys on selected LiveKit room events.
pub fn spawn_livekit_e2ee_event_resend(
    room: Room,
    mut events: tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
    context: PerParticipantE2eeContext,
) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                RoomEvent::Reconnected => {
                    let _ = send_per_participant_keys(
                        &room,
                        context.key_index,
                        &context.local_key,
                        None,
                    )
                    .await;
                }
                RoomEvent::ParticipantConnected(participant)
                | RoomEvent::TrackPublished { participant, .. }
                | RoomEvent::TrackSubscribed { participant, .. } => {
                    let target_device_id = participant
                        .identity()
                        .as_str()
                        .rsplit_once(':')
                        .map(|(_, device_id)| device_id.to_owned());
                    let _ = send_per_participant_keys(
                        &room,
                        context.key_index,
                        &context.local_key,
                        target_device_id.as_deref(),
                    )
                    .await;
                }
                _ => {}
            }
        }
    });
}

/// Read per-participant grace period from an env var.
pub fn per_participant_key_grace_period_from_env(var: &str, default_ms: u64) -> Duration {
    let grace_ms =
        std::env::var(var).ok().and_then(|value| value.parse::<u64>().ok()).unwrap_or(default_ms);
    Duration::from_millis(grace_ms)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        E2eeRoomOptionsProvider, derive_per_participant_key,
        per_participant_key_grace_period_from_env,
    };
    use crate::LiveKitRoomOptionsProvider;

    #[test]
    fn derive_per_participant_key_returns_expected_length() {
        let key = derive_per_participant_key();

        assert_eq!(key.len(), 16);
        assert!(key.iter().any(|byte| *byte != 0));
    }

    #[test]
    fn room_options_provider_without_context_leaves_encryption_disabled() {
        let provider = E2eeRoomOptionsProvider { e2ee: None };
        let options = provider.room_options();

        assert!(options.encryption.is_none());
    }

    #[test]
    fn per_participant_key_grace_period_from_env_uses_default_on_missing_or_invalid_values() {
        let missing_var = "MATRIX_SDK_LIVEKIT_TEST_GRACE_PERIOD_MISSING";
        let invalid_var = "MATRIX_SDK_LIVEKIT_TEST_GRACE_PERIOD_INVALID";

        // SAFETY: Tests are the only callers using these dedicated variable names.
        unsafe { std::env::remove_var(missing_var) };
        // SAFETY: Tests are the only callers using these dedicated variable names.
        unsafe { std::env::set_var(invalid_var, "not-a-number") };

        assert_eq!(
            per_participant_key_grace_period_from_env(missing_var, 500),
            Duration::from_millis(500)
        );
        assert_eq!(
            per_participant_key_grace_period_from_env(invalid_var, 750),
            Duration::from_millis(750)
        );
    }

    #[test]
    fn per_participant_key_grace_period_from_env_uses_valid_value() {
        let var = "MATRIX_SDK_LIVEKIT_TEST_GRACE_PERIOD_VALID";

        // SAFETY: Tests are the only callers using these dedicated variable names.
        unsafe { std::env::set_var(var, "1200") };

        assert_eq!(
            per_participant_key_grace_period_from_env(var, 300),
            Duration::from_millis(1200)
        );
    }
}
