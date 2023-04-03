//! Olm types.

use wasm_bindgen::prelude::*;

use crate::{identifiers, impl_from_to_inner, vodozemac::Curve25519PublicKey};

/// Struct representing the state of our private cross signing keys,
/// it shows which private cross signing keys we have locally stored.
#[wasm_bindgen]
#[derive(Debug)]
pub struct CrossSigningStatus {
    inner: matrix_sdk_crypto::olm::CrossSigningStatus,
}

impl_from_to_inner!(matrix_sdk_crypto::olm::CrossSigningStatus => CrossSigningStatus);

#[wasm_bindgen]
impl CrossSigningStatus {
    /// Do we have the master key?
    #[wasm_bindgen(getter, js_name = "hasMaster")]
    pub fn has_master(&self) -> bool {
        self.inner.has_master
    }

    /// Do we have the self signing key? This one is necessary to sign
    /// our own devices.
    #[wasm_bindgen(getter, js_name = "hasSelfSigning")]
    pub fn has_self_signing(&self) -> bool {
        self.inner.has_self_signing
    }

    /// Do we have the user signing key? This one is necessary to sign
    /// other users.
    #[wasm_bindgen(getter, js_name = "hasUserSigning")]
    pub fn has_user_signing(&self) -> bool {
        self.inner.has_user_signing
    }
}

/// Inbound group session.
///
/// Inbound group sessions are used to exchange room messages between a group of
/// participants. Inbound group sessions are used to decrypt the room messages.
#[wasm_bindgen]
#[derive(Debug)]
pub struct InboundGroupSession {
    inner: matrix_sdk_crypto::olm::InboundGroupSession,
}

impl_from_to_inner!(matrix_sdk_crypto::olm::InboundGroupSession => InboundGroupSession);

#[wasm_bindgen]
impl InboundGroupSession {
    /// The room where this session is used in.
    #[wasm_bindgen(getter, js_name = "roomId")]
    pub fn room_id(&self) -> identifiers::RoomId {
        self.inner.room_id().to_owned().into()
    }

    /// The Curve25519 key of the sender of this session, as a
    /// [Curve25519PublicKey].
    #[wasm_bindgen(getter, js_name = "senderKey")]
    pub fn sender_key(&self) -> Curve25519PublicKey {
        self.inner.sender_key().into()
    }

    /// Returns the unique identifier for this session.
    #[wasm_bindgen(getter, js_name = "sessionId")]
    pub fn session_id(&self) -> String {
        self.inner.session_id().to_owned()
    }

    /// Has the session been imported from a file or server-side backup? As
    /// opposed to being directly received as an `m.room_key` event.
    #[wasm_bindgen(js_name = "hasBeenImported")]
    pub fn has_been_imported(&self) -> bool {
        self.inner.has_been_imported()
    }
}
