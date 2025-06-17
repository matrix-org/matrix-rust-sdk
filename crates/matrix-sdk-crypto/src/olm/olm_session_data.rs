use serde::{Deserialize, Serialize};

/// Data representing an Olm session.
#[derive(Debug, Serialize, Deserialize)]
pub struct OlmSessionData {
    /// The peer's Curve25519 identity key.
    pub peer_identity_key: String,
    /// Our ephemeral Curve25519 key used for this session.
    pub our_ephemeral_key: String,
    /// The session ID.
    pub session_id: String,
    /// The derived ratchet keys for sending messages.
    pub sending_ratchet_key: String,
    /// The derived ratchet keys for receiving messages.
    pub receiving_ratchet_key: String,
    /// The message counter for sending messages.
    pub sending_message_counter: u32,
    /// The message counter for receiving messages.
    pub receiving_message_counter: u32,
}

impl OlmSessionData {
    /// Create a new `OlmSessionData` instance.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        peer_identity_key: String,
        our_ephemeral_key: String,
        session_id: String,
        sending_ratchet_key: String,
        receiving_ratchet_key: String,
        sending_message_counter: u32,
        receiving_message_counter: u32,
    ) -> Self {
        Self {
            peer_identity_key,
            our_ephemeral_key,
            session_id,
            sending_ratchet_key,
            receiving_ratchet_key,
            sending_message_counter,
            receiving_message_counter,
        }
    }
}
