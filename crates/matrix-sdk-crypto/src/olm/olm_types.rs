use ruma::{OwnedDeviceId, OwnedUserId};
use vodozemac::{Curve25519PublicKey, Ed25519PublicKey, Ed25519Signature};

/// Represents the necessary cryptographic keys obtained from another user's device
/// to establish an Olm session.
#[derive(Debug, Clone)]
pub struct OlmPreKeyBundle {
    /// The user ID of the owner of the device.
    pub user_id: OwnedUserId,
    /// The ID of the device.
    pub device_id: OwnedDeviceId,
    /// The Curve25519 identity key of the device.
    pub identity_key: Curve25519PublicKey,
    /// The Ed25519 signing key of the device (used to verify signatures).
    pub signing_key: Ed25519PublicKey,
    /// The claimed one-time Curve25519 public key.
    pub one_time_key: Curve25519PublicKey,
    /// Optional: A signed pre-key (if one was advertised and claimed, distinct from the OTK).
    /// For many Olm flows, the "prekey" is the one-time key itself.
    /// This field is for cases where a longer-term signed prekey might be used.
    pub signed_pre_key: Option<Curve25519PublicKey>,
    /// Optional: Signature of the `signed_pre_key` by the device's signing key.
    pub pre_key_signature: Option<Ed25519Signature>,
}

/// Holds the result of a successful Olm decryption.
#[derive(Debug, Clone)]
pub struct DecryptedOlmEvent {
    /// The decrypted plaintext, typically a JSON string of the original event.
    pub plaintext: String,
    /// The user ID of the sender of the original event (from the `m.room.encrypted` envelope).
    pub sender_user_id: OwnedUserId,
    /// The device ID of the sender, if determinable (e.g., if a new session was created
    /// with a specific device, or if device information is available).
    /// This is the device that *sent* the Olm message.
    pub sender_device_id: Option<OwnedDeviceId>,
    /// The Curve25519 identity key of the sender's device (from the OlmV1 content).
    pub sender_identity_key: Curve25519PublicKey,
    /// The Curve25519 identity key of our own device (recipient).
    pub recipient_identity_key: Curve25519PublicKey,
    /// The Ed25519 key that the sender claimed was ours (the recipient's).
    /// This is from the `m.room.encrypted` content's `keys` field.
    pub recipient_claimed_ed25519_key: String,
}
