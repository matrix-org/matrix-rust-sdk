// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::{
    cmp::Ordering,
    fmt,
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::SeqCst},
    },
};

use ruma::{
    DeviceKeyAlgorithm, OwnedRoomId, RoomId, events::room::history_visibility::HistoryVisibility,
    serde::JsonObject,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use vodozemac::{
    Curve25519PublicKey, Ed25519PublicKey, PickleError,
    megolm::{
        DecryptedMessage, DecryptionError, InboundGroupSession as InnerSession,
        InboundGroupSessionPickle, MegolmMessage, SessionConfig, SessionOrdering,
    },
};

use super::{
    BackedUpRoomKey, ExportedRoomKey, OutboundGroupSession, SenderData, SenderDataType,
    SessionCreationError, SessionKey,
};
#[cfg(doc)]
use crate::types::events::room_key::RoomKeyContent;
use crate::{
    error::{EventError, MegolmResult},
    olm::group_sessions::forwarder_data::ForwarderData,
    types::{
        EventEncryptionAlgorithm, SigningKeys, deserialize_curve_key,
        events::{
            forwarded_room_key::{
                ForwardedMegolmV1AesSha2Content, ForwardedMegolmV2AesSha2Content,
                ForwardedRoomKeyContent,
            },
            olm_v1::DecryptedForwardedRoomKeyEvent,
            room::encrypted::{EncryptedEvent, RoomEventEncryptionScheme},
            room_key,
        },
        room_history::HistoricRoomKey,
        serialize_curve_key,
    },
};
// TODO: add creation times to the inbound group sessions so we can export
// sessions that were created between some time period, this should only be set
// for non-imported sessions.

/// Information about the creator of an inbound group session.
#[derive(Clone)]
pub(crate) struct SessionCreatorInfo {
    /// The Curve25519 identity key of the session creator.
    ///
    /// If the session was received directly from its creator device through an
    /// `m.room_key` event (and therefore, session sender == session creator),
    /// this key equals the Curve25519 device identity key of that device. Since
    /// this key is one of three keys used to establish the Olm session through
    /// which encrypted to-device messages (including `m.room_key`) are sent,
    /// this constitutes a proof that this inbound group session is owned by
    /// that particular Curve25519 key.
    ///
    /// However, if the session was simply forwarded to us in an
    /// `m.forwarded_room_key` event (in which case sender != creator), this key
    /// is just a *claim* made by the session sender of what the actual creator
    /// device is.
    pub curve25519_key: Curve25519PublicKey,

    /// A mapping of DeviceKeyAlgorithm to the public signing keys of the
    /// [`Device`] that sent us the session.
    ///
    /// If the session was received directly from the creator via an
    /// `m.room_key` event, this map is taken from the plaintext value of
    /// the decrypted Olm event, and is a copy of the
    /// [`DecryptedOlmV1Event::keys`] field as defined in the [spec].
    ///
    /// If the session was forwarded to us using an `m.forwarded_room_key`, this
    /// map is a copy of the claimed Ed25519 key from the content of the
    /// event.
    ///
    /// [spec]: https://spec.matrix.org/unstable/client-server-api/#molmv1curve25519-aes-sha2
    pub signing_keys: Arc<SigningKeys<DeviceKeyAlgorithm>>,
}

/// A structure representing an inbound group session.
///
/// Inbound group sessions, also known as "room keys", are used to facilitate
/// the exchange of room messages among a group of participants. The inbound
/// variant of the group session is used to decrypt the room messages.
///
/// This struct wraps the [vodozemac] type of the same name, and adds additional
/// Matrix-specific data to it. Additionally, the wrapper ensures thread-safe
/// access of the vodozemac type.
///
/// [vodozemac]: https://matrix-org.github.io/vodozemac/vodozemac/index.html
///
/// ## Structures representing serialised versions of an `InboundGroupSession`
///
/// This crate contains a number of structures which are used for exporting or
/// sharing `InboundGroupSession` between users or devices, in different
/// circumstances. The following is an attempt to catalogue them.
///
/// 1. First, we have the contents of an `m.room_key` to-device message (i.e., a
///    [`RoomKeyContent`]. `RoomKeyContent` is unusual in that it can be created
///    only by the original creator of the session (i.e., someone in possession
///    of the corresponding [`OutboundGroupSession`]), since the embedded
///    `session_key` is self-signed.
///
///    `RoomKeyContent` does **not** include any information about the creator
///    of the session (such as the creator's public device keys), since it is
///    assumed that the original creator of the session is the same as the
///    device sending the to-device message; it is therefore implied by the Olm
///    channel used to send the message.
///
///    All the other structs in this list include a `sender_key` field which
///    contains the Curve25519 key belonging to the device which created the
///    Megolm session (at least, according to the creator of the struct); they
///    also include the Ed25519 key, though the exact serialisation mechanism
///    varies.
///
/// 2. Next, we have the contents of an `m.forwarded_room_key` message (i.e. a
///    [`ForwardedRoomKeyContent`]). This modifies `RoomKeyContent` by (a) using
///    a `session_key` which is not self-signed, (b) adding a `sender_key` field
///    as mentioned above, (c) adding a `sender_claimed_ed25519_key` field
///    containing the original sender's Ed25519 key; (d) adding a
///    `forwarding_curve25519_key_chain` field, which is intended to be used
///    when the key is re-forwarded, but in practice is of little use.
///
/// 3. [`ExportedRoomKey`] is very similar to `ForwardedRoomKeyContent`. The
///    only difference is that the original sender's Ed25519 key is embedded in
///    a `sender_claimed_keys` map rather than a top-level
///    `sender_claimed_ed25519_key` field.
///
/// 4. [`BackedUpRoomKey`] is essentially the same as `ExportedRoomKey`, but
///    lacks explicit `room_id` and `session_id` (since those are implied by
///    other parts of the key backup structure).
///
/// 5. [`HistoricRoomKey`] is also similar to `ExportedRoomKey`, but omits
///    `forwarding_curve25519_key_chain` (since it has not been useful in
///    practice) and `shared_history` (because any key being shared via that
///    mechanism is inherently suitable for sharing with other users).
///
/// | Type     | Self-signed room key | `room_id`, `session_id` | `sender_key` | Sender's Ed25519 key | `forwarding _curve25519 _key _chain` | `shared _history` |
/// |----------|----------------------|-------------------------|--------------|----------------------|------------------------------------|------------------|
/// | [`RoomKeyContent`]          | ✅ | ✅                       | ❌            | ❌                    | ❌                                  | ✅                |
/// | [`ForwardedRoomKeyContent`] | ❌ | ✅                       | ✅            | `sender_claimed_ed25519_key` | ✅                          | ✅                |
/// | [`ExportedRoomKey`]         | ❌ | ✅                       | ✅            | `sender_claimed_keys` | ✅                                 | ✅                |
/// | [`BackedUpRoomKey`]         | ❌ | ❌                       | ✅            | `sender_claimed_keys` | ✅                                 | ✅                |
/// | [`HistoricRoomKey`]         | ❌ | ✅                       | ✅            | `sender_claimed_keys` | ❌                                 | ❌                |
#[derive(Clone)]
pub struct InboundGroupSession {
    inner: Arc<Mutex<InnerSession>>,

    /// A copy of [`InnerSession::session_id`] to avoid having to acquire a lock
    /// to get to the session ID.
    session_id: Arc<str>,

    /// A copy of [`InnerSession::first_known_index`] to avoid having to acquire
    /// a lock to get to the first known index.
    first_known_index: u32,

    /// Information about the creator of the [`InboundGroupSession`] ("room
    /// key"). The trustworthiness of the information in this field depends
    /// on how the session was received.
    pub(crate) creator_info: SessionCreatorInfo,

    /// Information about the sender of this session and how much we trust that
    /// information. Holds the information we have about the device that created
    /// the session, or, if we can use that device information to find the
    /// sender's cross-signing identity, holds the user ID and cross-signing
    /// key.
    pub sender_data: SenderData,

    /// If this session was shared-on-invite as part of an [MSC4268] key bundle,
    /// information about the user who forwarded us the session information.
    /// This is distinct from [`InboundGroupSession::sender_data`].
    ///
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    pub forwarder_data: Option<ForwarderData>,

    /// The Room this GroupSession belongs to
    pub room_id: OwnedRoomId,

    /// A flag recording whether the `InboundGroupSession` was received directly
    /// as a `m.room_key` event or indirectly via a forward or file import.
    ///
    /// If the session is considered to be imported, the information contained
    /// in the `InboundGroupSession::creator_info` field is not proven to be
    /// correct.
    imported: bool,

    /// The messaging algorithm of this [`InboundGroupSession`] as defined by
    /// the [spec]. Will be one of the `m.megolm.*` algorithms.
    ///
    /// [spec]: https://spec.matrix.org/unstable/client-server-api/#messaging-algorithms
    algorithm: Arc<EventEncryptionAlgorithm>,

    /// The history visibility of the room at the time when the room key was
    /// created.
    history_visibility: Arc<Option<HistoryVisibility>>,

    /// Was this room key backed up to the server.
    backed_up: Arc<AtomicBool>,

    /// Whether this [`InboundGroupSession`] can be shared with users who are
    /// invited to the room in the future, allowing access to history, as
    /// defined in [MSC3061].
    ///
    /// [MSC3061]: https://github.com/matrix-org/matrix-spec-proposals/pull/3061
    shared_history: bool,
}

impl InboundGroupSession {
    /// Create a new inbound group session for the given room.
    ///
    /// These sessions are used to decrypt room messages.
    ///
    /// # Arguments
    ///
    /// * `sender_key` - The public Curve25519 key of the account that sent us
    ///   the session.
    ///
    /// * `signing_key` - The public Ed25519 key of the account that sent us the
    ///   session.
    ///
    /// * `room_id` - The id of the room that the session is used in.
    ///
    /// * `session_key` - The private session key that is used to decrypt
    ///   messages.
    ///
    /// * `sender_data` - Information about the sender of the to-device message
    ///   that established this session.
    ///
    /// * `forwarder_data` - If present, indicates this session was received via
    ///   an [MSC4268] room key bundle, and provides information about the
    ///   forwarder of this bundle.
    ///
    /// * `encryption_algorithm` - The [`EventEncryptionAlgorithm`] that should
    ///   be used when messages are being decrypted. The method will return an
    ///   [`SessionCreationError::Algorithm`] error if an algorithm we do not
    ///   support is given,
    ///
    /// * `history_visibility` - The history visibility of the room at the time
    ///   the matching [`OutboundGroupSession`] was created. This is only set if
    ///   we are the crator of this  [`InboundGroupSession`]. Sessinons that are
    ///   received from other devices use the  `shared_history` flag instead.
    ///
    /// * `shared_history` - Whether this [`InboundGroupSession`] can be shared
    ///   with users who are invited to the room in the future, allowing access
    ///   to history, as defined in [MSC3061]. This flag is a surjection of the
    ///   history visibility of the room.
    ///
    /// [MSC3061]: https://github.com/matrix-org/matrix-spec-proposals/pull/3061
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sender_key: Curve25519PublicKey,
        signing_key: Ed25519PublicKey,
        room_id: &RoomId,
        session_key: &SessionKey,
        sender_data: SenderData,
        forwarder_data: Option<ForwarderData>,
        encryption_algorithm: EventEncryptionAlgorithm,
        history_visibility: Option<HistoryVisibility>,
        shared_history: bool,
    ) -> Result<Self, SessionCreationError> {
        let config = OutboundGroupSession::session_config(&encryption_algorithm)?;

        let session = InnerSession::new(session_key, config);
        let session_id = session.session_id();
        let first_known_index = session.first_known_index();

        let mut keys = SigningKeys::new();
        keys.insert(DeviceKeyAlgorithm::Ed25519, signing_key.into());

        Ok(InboundGroupSession {
            inner: Arc::new(Mutex::new(session)),
            history_visibility: history_visibility.into(),
            session_id: session_id.into(),
            first_known_index,
            creator_info: SessionCreatorInfo {
                curve25519_key: sender_key,
                signing_keys: keys.into(),
            },
            sender_data,
            forwarder_data,
            room_id: room_id.into(),
            imported: false,
            algorithm: encryption_algorithm.into(),
            backed_up: AtomicBool::new(false).into(),
            shared_history,
        })
    }

    /// Create a new [`InboundGroupSession`] from a `m.room_key` event with an
    /// `m.megolm.v1.aes-sha2` content.
    ///
    /// The `m.room_key` event **must** have been encrypted using the
    /// `m.olm.v1.curve25519-aes-sha2` algorithm and the `sender_key` **must**
    /// be the long-term [`Curve25519PublicKey`] that was used to establish
    /// the 1-to-1 Olm session.
    ///
    /// The `signing_key` **must** be the [`Ed25519PublicKey`] contained in the
    /// `keys` field of the [decrypted payload].
    ///
    /// [decrypted payload]: https://spec.matrix.org/unstable/client-server-api/#molmv1curve25519-aes-sha2
    pub fn from_room_key_content(
        sender_key: Curve25519PublicKey,
        signing_key: Ed25519PublicKey,
        content: &room_key::MegolmV1AesSha2Content,
    ) -> Result<Self, SessionCreationError> {
        let room_key::MegolmV1AesSha2Content {
            room_id,
            session_id: _,
            session_key,
            shared_history,
            ..
        } = content;

        Self::new(
            sender_key,
            signing_key,
            room_id,
            session_key,
            SenderData::unknown(),
            None,
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            None,
            *shared_history,
        )
    }

    /// Create a new [`InboundGroupSession`] from an exported version of the
    /// group session.
    ///
    /// Most notably this can be called with an [`ExportedRoomKey`] from a
    /// previous [`InboundGroupSession::export()`] call.
    pub fn from_export(exported_session: &ExportedRoomKey) -> Result<Self, SessionCreationError> {
        Self::try_from(exported_session)
    }

    /// Create a new [`InboundGroupSession`] which is a copy of this one, except
    /// that its Megolm ratchet is replaced with a copy of that from another
    /// [`InboundGroupSession`].
    ///
    /// This can be useful, for example, when we receive a new copy of the room
    /// key, but at an earlier ratchet index.
    ///
    /// # Panics
    ///
    /// If the two sessions are for different room IDs, or have different
    /// session IDs, this function will panic. It is up to the caller to ensure
    /// that it only attempts to merge related sessions.
    pub(crate) fn with_ratchet(mut self, other: &InboundGroupSession) -> Self {
        if self.session_id != other.session_id {
            panic!(
                "Attempt to merge Megolm sessions with different session IDs: {} vs {}",
                self.session_id, other.session_id
            );
        }
        if self.room_id != other.room_id {
            panic!(
                "Attempt to merge Megolm sessions with different room IDs: {} vs {}",
                self.room_id, other.room_id,
            );
        }
        self.inner = other.inner.clone();
        self.first_known_index = other.first_known_index;
        self
    }

    /// Convert the [`InboundGroupSession`] into a
    /// [`PickledInboundGroupSession`] which can be serialized.
    pub async fn pickle(&self) -> PickledInboundGroupSession {
        let pickle = self.inner.lock().await.pickle();

        PickledInboundGroupSession {
            pickle,
            sender_key: self.creator_info.curve25519_key,
            signing_key: (*self.creator_info.signing_keys).clone(),
            sender_data: self.sender_data.clone(),
            forwarder_data: self.forwarder_data.clone(),
            room_id: self.room_id().to_owned(),
            imported: self.imported,
            backed_up: self.backed_up(),
            history_visibility: self.history_visibility.as_ref().clone(),
            algorithm: (*self.algorithm).to_owned(),
            shared_history: self.shared_history,
        }
    }

    /// Export this session at the first known message index.
    ///
    /// If only a limited part of this session should be exported use
    /// [`InboundGroupSession::export_at_index()`].
    pub async fn export(&self) -> ExportedRoomKey {
        self.export_at_index(self.first_known_index()).await
    }

    /// Get the sender key that this session was received from.
    pub fn sender_key(&self) -> Curve25519PublicKey {
        self.creator_info.curve25519_key
    }

    /// Has the session been backed up to the server.
    pub fn backed_up(&self) -> bool {
        self.backed_up.load(SeqCst)
    }

    /// Reset the backup state of the inbound group session.
    pub fn reset_backup_state(&self) {
        self.backed_up.store(false, SeqCst)
    }

    /// For testing, allow to manually mark this GroupSession to have been
    /// backed up
    pub fn mark_as_backed_up(&self) {
        self.backed_up.store(true, SeqCst)
    }

    /// Get the map of signing keys this session was received from.
    pub fn signing_keys(&self) -> &SigningKeys<DeviceKeyAlgorithm> {
        &self.creator_info.signing_keys
    }

    /// Export this session at the given message index.
    pub async fn export_at_index(&self, message_index: u32) -> ExportedRoomKey {
        let message_index = std::cmp::max(self.first_known_index(), message_index);

        let session_key =
            self.inner.lock().await.export_at(message_index).expect("Can't export session");

        ExportedRoomKey {
            algorithm: self.algorithm().to_owned(),
            room_id: self.room_id().to_owned(),
            sender_key: self.creator_info.curve25519_key,
            session_id: self.session_id().to_owned(),
            forwarding_curve25519_key_chain: vec![],
            sender_claimed_keys: (*self.creator_info.signing_keys).clone(),
            session_key,
            shared_history: self.shared_history,
        }
    }

    /// Restore a Session from a previously pickled string.
    ///
    /// Returns the restored group session or a `UnpicklingError` if there
    /// was an error.
    ///
    /// # Arguments
    ///
    /// * `pickle` - The pickled version of the `InboundGroupSession`.
    ///
    /// * `pickle_mode` - The mode that was used to pickle the session, either
    ///   an unencrypted mode or an encrypted using passphrase.
    pub fn from_pickle(pickle: PickledInboundGroupSession) -> Result<Self, PickleError> {
        let PickledInboundGroupSession {
            pickle,
            sender_key,
            signing_key,
            sender_data,
            forwarder_data,
            room_id,
            imported,
            backed_up,
            history_visibility,
            algorithm,
            shared_history,
        } = pickle;

        let session: InnerSession = pickle.into();
        let first_known_index = session.first_known_index();
        let session_id = session.session_id();

        Ok(InboundGroupSession {
            inner: Mutex::new(session).into(),
            session_id: session_id.into(),
            creator_info: SessionCreatorInfo {
                curve25519_key: sender_key,
                signing_keys: signing_key.into(),
            },
            sender_data,
            forwarder_data,
            history_visibility: history_visibility.into(),
            first_known_index,
            room_id,
            backed_up: AtomicBool::from(backed_up).into(),
            algorithm: algorithm.into(),
            imported,
            shared_history,
        })
    }

    /// The room where this session is used in.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Returns the unique identifier for this session.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The algorithm that this inbound group session is using to decrypt
    /// events.
    pub fn algorithm(&self) -> &EventEncryptionAlgorithm {
        &self.algorithm
    }

    /// Get the first message index we know how to decrypt.
    pub fn first_known_index(&self) -> u32 {
        self.first_known_index
    }

    /// Has the session been imported from a file or server-side backup? As
    /// opposed to being directly received as an `m.room_key` event.
    pub fn has_been_imported(&self) -> bool {
        self.imported
    }

    /// Check if the [`InboundGroupSession`] is better than the given other
    /// [`InboundGroupSession`]
    #[deprecated(
        note = "Sessions cannot be compared on a linear scale. Consider calling `compare_ratchet`, as well as comparing the `sender_data`."
    )]
    pub async fn compare(&self, other: &InboundGroupSession) -> SessionOrdering {
        match self.compare_ratchet(other).await {
            SessionOrdering::Equal => {
                match self.sender_data.compare_trust_level(&other.sender_data) {
                    Ordering::Less => SessionOrdering::Worse,
                    Ordering::Equal => SessionOrdering::Equal,
                    Ordering::Greater => SessionOrdering::Better,
                }
            }
            result => result,
        }
    }

    /// Check if the [`InboundGroupSession`]'s ratchet index is better than that
    /// of the given other [`InboundGroupSession`].
    ///
    /// If the two sessions are not connected (i.e., they are from different
    /// senders, or if advancing the ratchets to the same index does not
    /// give the same ratchet value), returns [`SessionOrdering::Unconnected`].
    ///
    /// Otherwise, returns [`SessionOrdering::Equal`],
    /// [`SessionOrdering::Better`], or [`SessionOrdering::Worse`] respectively
    /// depending on whether this session's first known index is equal to,
    /// lower than, or higher than, that of `other`.
    pub async fn compare_ratchet(&self, other: &InboundGroupSession) -> SessionOrdering {
        // If this is the same object the ordering is the same, we can't compare because
        // we would deadlock while trying to acquire the same lock twice.
        if Arc::ptr_eq(&self.inner, &other.inner) {
            SessionOrdering::Equal
        } else if self.sender_key() != other.sender_key()
            || self.signing_keys() != other.signing_keys()
            || self.algorithm() != other.algorithm()
            || self.room_id() != other.room_id()
        {
            SessionOrdering::Unconnected
        } else {
            let mut other_inner = other.inner.lock().await;
            self.inner.lock().await.compare(&mut other_inner)
        }
    }

    /// Decrypt the given ciphertext.
    ///
    /// Returns the decrypted plaintext or an `DecryptionError` if
    /// decryption failed.
    ///
    /// # Arguments
    ///
    /// * `message` - The message that should be decrypted.
    pub(crate) async fn decrypt_helper(
        &self,
        message: &MegolmMessage,
    ) -> Result<DecryptedMessage, DecryptionError> {
        self.inner.lock().await.decrypt(message)
    }

    /// Export the inbound group session into a format that can be uploaded to
    /// the server as a backup.
    pub async fn to_backup(&self) -> BackedUpRoomKey {
        self.export().await.into()
    }

    /// Decrypt an event from a room timeline.
    ///
    /// # Arguments
    ///
    /// * `event` - The event that should be decrypted.
    pub async fn decrypt(&self, event: &EncryptedEvent) -> MegolmResult<(JsonObject, u32)> {
        let decrypted = match &event.content.scheme {
            RoomEventEncryptionScheme::MegolmV1AesSha2(c) => {
                self.decrypt_helper(&c.ciphertext).await?
            }
            #[cfg(feature = "experimental-algorithms")]
            RoomEventEncryptionScheme::MegolmV2AesSha2(c) => {
                self.decrypt_helper(&c.ciphertext).await?
            }
            RoomEventEncryptionScheme::Unknown(_) => {
                return Err(EventError::UnsupportedAlgorithm.into());
            }
        };

        let plaintext = String::from_utf8_lossy(&decrypted.plaintext);

        let mut decrypted_object = serde_json::from_str::<JsonObject>(&plaintext)?;

        let server_ts: i64 = event.origin_server_ts.0.into();

        decrypted_object.insert("sender".to_owned(), event.sender.to_string().into());
        decrypted_object.insert("event_id".to_owned(), event.event_id.to_string().into());
        decrypted_object.insert("origin_server_ts".to_owned(), server_ts.into());

        let room_id = decrypted_object
            .get("room_id")
            .and_then(|r| r.as_str().and_then(|r| RoomId::parse(r).ok()));

        // Check that we have a room id and that the event wasn't forwarded from
        // another room.
        if room_id.as_deref() != Some(self.room_id()) {
            return Err(EventError::MismatchedRoom(self.room_id().to_owned(), room_id).into());
        }

        decrypted_object.insert(
            "unsigned".to_owned(),
            serde_json::to_value(&event.unsigned).unwrap_or_default(),
        );

        if let Some(decrypted_content) =
            decrypted_object.get_mut("content").and_then(|c| c.as_object_mut())
            && !decrypted_content.contains_key("m.relates_to")
            && let Some(relation) = &event.content.relates_to
        {
            decrypted_content.insert("m.relates_to".to_owned(), relation.to_owned());
        }

        Ok((decrypted_object, decrypted.message_index))
    }

    /// For test only, mark this session as imported.
    #[cfg(test)]
    pub(crate) fn mark_as_imported(&mut self) {
        self.imported = true;
    }

    /// Return the [`SenderDataType`] of our [`SenderData`]. This is used during
    /// serialization, to allow us to store the type in a separate queryable
    /// column/property.
    pub fn sender_data_type(&self) -> SenderDataType {
        self.sender_data.to_type()
    }

    /// Whether this [`InboundGroupSession`] can be shared with users who are
    /// invited to the room in the future, allowing access to history, as
    /// defined in [MSC3061].
    ///
    /// [MSC3061]: https://github.com/matrix-org/matrix-spec-proposals/pull/3061
    pub fn shared_history(&self) -> bool {
        self.shared_history
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for InboundGroupSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboundGroupSession").field("session_id", &self.session_id()).finish()
    }
}

impl PartialEq for InboundGroupSession {
    fn eq(&self, other: &Self) -> bool {
        self.session_id() == other.session_id()
    }
}

/// A pickled version of an `InboundGroupSession`.
///
/// Holds all the information that needs to be stored in a database to restore
/// an InboundGroupSession.
#[derive(Serialize, Deserialize)]
#[allow(missing_debug_implementations)]
pub struct PickledInboundGroupSession {
    /// The pickle string holding the InboundGroupSession.
    pub pickle: InboundGroupSessionPickle,
    /// The public Curve25519 key of the account that sent us the session
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    pub sender_key: Curve25519PublicKey,
    /// The public ed25519 key of the account that sent us the session.
    pub signing_key: SigningKeys<DeviceKeyAlgorithm>,
    /// Information on the device/sender who sent us this session
    #[serde(default)]
    pub sender_data: SenderData,
    /// Information on the device/sender who forwarded us this session
    #[serde(default)]
    pub forwarder_data: Option<ForwarderData>,
    /// The id of the room that the session is used in.
    pub room_id: OwnedRoomId,
    /// Flag remembering if the session was directly sent to us by the sender
    /// or if it was imported.
    pub imported: bool,
    /// Flag remembering if the session has been backed up.
    #[serde(default)]
    pub backed_up: bool,
    /// History visibility of the room when the session was created.
    pub history_visibility: Option<HistoryVisibility>,
    /// The algorithm of this inbound group session.
    #[serde(default = "default_algorithm")]
    pub algorithm: EventEncryptionAlgorithm,
    /// Whether this [`InboundGroupSession`] can be shared with users who are
    /// invited to the room in the future, allowing access to history, as
    /// defined in [MSC3061].
    ///
    /// [MSC3061]: https://github.com/matrix-org/matrix-spec-proposals/pull/3061
    #[serde(default)]
    pub shared_history: bool,
}

fn default_algorithm() -> EventEncryptionAlgorithm {
    EventEncryptionAlgorithm::MegolmV1AesSha2
}

impl HistoricRoomKey {
    /// Converts a `HistoricRoomKey` into an `InboundGroupSession`.
    ///
    /// This method takes the current `HistoricRoomKey` instance and attempts to
    /// create an `InboundGroupSession` from it. The `forwarder_data` parameter
    /// provides information about the user or device that forwarded the session
    /// information. This is normally distinct from the original sender of the
    /// session.
    ///
    /// # Arguments
    ///
    /// * `forwarder_data` - A reference to a `SenderData` object containing
    ///   information about the forwarder of the session.
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing the newly created `InboundGroupSession` on
    /// success, or a `SessionCreationError` if the conversion fails.
    ///
    /// # Errors
    ///
    /// This method will return a `SessionCreationError` if the session
    /// configuration for the given algorithm cannot be determined.
    pub fn try_into_inbound_group_session(
        &self,
        forwarder_data: &ForwarderData,
    ) -> Result<InboundGroupSession, SessionCreationError> {
        let HistoricRoomKey {
            algorithm,
            room_id,
            sender_key,
            session_id,
            session_key,
            sender_claimed_keys,
        } = self;

        let config = OutboundGroupSession::session_config(algorithm)?;
        let session = InnerSession::import(session_key, config);
        let first_known_index = session.first_known_index();

        Ok(InboundGroupSession {
            inner: Mutex::new(session).into(),
            session_id: session_id.to_owned().into(),
            creator_info: SessionCreatorInfo {
                curve25519_key: *sender_key,
                signing_keys: sender_claimed_keys.to_owned().into(),
            },
            // TODO: How do we remember that this is a historic room key and events decrypted using
            // this room key should always show some form of warning.
            sender_data: SenderData::default(),
            forwarder_data: Some(forwarder_data.clone()),
            history_visibility: None.into(),
            first_known_index,
            room_id: room_id.to_owned(),
            imported: true,
            algorithm: algorithm.to_owned().into(),
            backed_up: AtomicBool::from(false).into(),
            shared_history: true,
        })
    }
}

impl TryFrom<&ExportedRoomKey> for InboundGroupSession {
    type Error = SessionCreationError;

    fn try_from(key: &ExportedRoomKey) -> Result<Self, Self::Error> {
        let ExportedRoomKey {
            algorithm,
            room_id,
            sender_key,
            session_id,
            session_key,
            sender_claimed_keys,
            forwarding_curve25519_key_chain: _,
            shared_history,
        } = key;

        let config = OutboundGroupSession::session_config(algorithm)?;
        let session = InnerSession::import(session_key, config);
        let first_known_index = session.first_known_index();

        Ok(InboundGroupSession {
            inner: Mutex::new(session).into(),
            session_id: session_id.to_owned().into(),
            creator_info: SessionCreatorInfo {
                curve25519_key: *sender_key,
                signing_keys: sender_claimed_keys.to_owned().into(),
            },
            // TODO: In future, exported keys should contain sender data that we can use here.
            // See https://github.com/matrix-org/matrix-rust-sdk/issues/3548
            sender_data: SenderData::default(),
            forwarder_data: None,
            history_visibility: None.into(),
            first_known_index,
            room_id: room_id.to_owned(),
            imported: true,
            algorithm: algorithm.to_owned().into(),
            backed_up: AtomicBool::from(false).into(),
            shared_history: *shared_history,
        })
    }
}

impl From<&ForwardedMegolmV1AesSha2Content> for InboundGroupSession {
    fn from(value: &ForwardedMegolmV1AesSha2Content) -> Self {
        let session = InnerSession::import(&value.session_key, SessionConfig::version_1());
        let session_id = session.session_id().into();
        let first_known_index = session.first_known_index();

        InboundGroupSession {
            inner: Mutex::new(session).into(),
            session_id,
            creator_info: SessionCreatorInfo {
                curve25519_key: value.claimed_sender_key,
                signing_keys: SigningKeys::from([(
                    DeviceKeyAlgorithm::Ed25519,
                    value.claimed_ed25519_key.into(),
                )])
                .into(),
            },
            // In future, exported keys should contain sender data that we can use here.
            // See https://github.com/matrix-org/matrix-rust-sdk/issues/3548
            sender_data: SenderData::default(),
            forwarder_data: None,
            history_visibility: None.into(),
            first_known_index,
            room_id: value.room_id.to_owned(),
            imported: true,
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2.into(),
            backed_up: AtomicBool::from(false).into(),
            shared_history: false,
        }
    }
}

impl From<&ForwardedMegolmV2AesSha2Content> for InboundGroupSession {
    fn from(value: &ForwardedMegolmV2AesSha2Content) -> Self {
        let session = InnerSession::import(&value.session_key, SessionConfig::version_2());
        let session_id = session.session_id().into();
        let first_known_index = session.first_known_index();

        InboundGroupSession {
            inner: Mutex::new(session).into(),
            session_id,
            creator_info: SessionCreatorInfo {
                curve25519_key: value.claimed_sender_key,
                signing_keys: value.claimed_signing_keys.to_owned().into(),
            },
            // In future, exported keys should contain sender data that we can use here.
            // See https://github.com/matrix-org/matrix-rust-sdk/issues/3548
            sender_data: SenderData::default(),
            forwarder_data: None,
            history_visibility: None.into(),
            first_known_index,
            room_id: value.room_id.to_owned(),
            imported: true,
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2.into(),
            backed_up: AtomicBool::from(false).into(),
            shared_history: false,
        }
    }
}

impl TryFrom<&DecryptedForwardedRoomKeyEvent> for InboundGroupSession {
    type Error = SessionCreationError;

    fn try_from(value: &DecryptedForwardedRoomKeyEvent) -> Result<Self, Self::Error> {
        match &value.content {
            ForwardedRoomKeyContent::MegolmV1AesSha2(c) => Ok(Self::from(c.deref())),
            #[cfg(feature = "experimental-algorithms")]
            ForwardedRoomKeyContent::MegolmV2AesSha2(c) => Ok(Self::from(c.deref())),
            ForwardedRoomKeyContent::Unknown(c) => {
                Err(SessionCreationError::Algorithm(c.algorithm.to_owned()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;
    use insta::{assert_json_snapshot, with_settings};
    use matrix_sdk_test::async_test;
    use ruma::{
        DeviceId, UserId, device_id, events::room::history_visibility::HistoryVisibility,
        owned_room_id, room_id, user_id,
    };
    use serde_json::json;
    use similar_asserts::assert_eq;
    use vodozemac::{
        Curve25519PublicKey, Ed25519PublicKey,
        megolm::{SessionKey, SessionOrdering},
    };

    use crate::{
        Account,
        olm::{BackedUpRoomKey, ExportedRoomKey, InboundGroupSession, KnownSenderData, SenderData},
        types::{EventEncryptionAlgorithm, events::room_key},
    };

    fn alice_id() -> &'static UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> &'static DeviceId {
        device_id!("ALICEDEVICE")
    }

    #[async_test]
    async fn test_pickle_snapshot() {
        let account = Account::new(alice_id());
        let room_id = room_id!("!test:localhost");
        let (_, session) = account.create_group_session_pair_with_defaults(room_id).await;

        let pickle = session.pickle().await;

        with_settings!({prepend_module_to_snapshot => false}, {
            assert_json_snapshot!(
                "InboundGroupSession__test_pickle_snapshot__regression",
                pickle,
                {
                    ".pickle.initial_ratchet.inner" => "[ratchet]",
                    ".pickle.signing_key" => "[signing_key]",
                    ".sender_key" => "[sender_key]",
                    ".signing_key.ed25519" => "[ed25519_key]",
                }
            );
        });
    }

    #[async_test]
    async fn test_can_deserialise_pickled_session_without_sender_data() {
        // Given the raw JSON for a picked inbound group session without any sender_data
        let pickle = r#"
        {
            "pickle": {
                "initial_ratchet": {
                    "inner": [ 124, 251, 213, 204, 108, 247, 54, 7, 179, 162, 15, 107, 154, 215,
                               220, 46, 123, 113, 120, 162, 225, 246, 237, 203, 125, 102, 190, 212,
                               229, 195, 136, 185, 26, 31, 77, 140, 144, 181, 152, 177, 46, 105,
                               202, 6, 53, 158, 157, 170, 31, 155, 130, 87, 214, 110, 143, 55, 68,
                               138, 41, 35, 242, 230, 194, 15, 16, 145, 116, 94, 89, 35, 79, 145,
                               245, 117, 204, 173, 166, 178, 49, 131, 143, 61, 61, 15, 211, 167, 17,
                               2, 79, 110, 149, 200, 223, 23, 185, 200, 29, 64, 55, 39, 147, 167,
                               205, 224, 159, 101, 218, 249, 203, 30, 175, 174, 48, 252, 40, 131,
                               52, 135, 91, 57, 211, 96, 105, 58, 55, 68, 250, 24 ],
                    "counter": 0
                },
                "signing_key": [ 93, 185, 171, 61, 173, 100, 51, 9, 157, 180, 214, 39, 131, 80, 118,
                                 130, 199, 232, 163, 197, 45, 23, 227, 100, 151, 59, 19, 102, 38,
                                 149, 43, 38 ],
                "signing_key_verified": true,
                "config": {
                  "version": "V1"
                }
            },
            "sender_key": "AmM1DvVJarsNNXVuX7OarzfT481N37GtDwvDVF0RcR8",
            "signing_key": {
                "ed25519": "wTRTdz4rn4EY+68cKPzpMdQ6RAlg7T8cbTmEjaXuUww"
            },
            "room_id": "!test:localhost",
            "forwarding_chains": ["tb6kQKjk+SJl2KnfQ0lKVOZl6gDFMcsb9HcUP9k/4hc"],
            "imported": false,
            "backed_up": false,
            "history_visibility": "shared",
            "algorithm": "m.megolm.v1.aes-sha2"
        }
        "#;

        // When we deserialise it to from JSON
        let deserialized = serde_json::from_str(pickle).unwrap();

        // And unpickle it
        let unpickled = InboundGroupSession::from_pickle(deserialized).unwrap();

        // Then it was parsed correctly
        assert_eq!(unpickled.session_id(), "XbmrPa1kMwmdtNYng1B2gsfoo8UtF+NklzsTZiaVKyY");

        // And we populated the InboundGroupSession's sender_data with a default value,
        // with legacy_session set to true.
        assert_let!(
            SenderData::UnknownDevice { legacy_session, owner_check_failed } =
                unpickled.sender_data
        );
        assert!(legacy_session);
        assert!(!owner_check_failed);
    }

    #[async_test]
    async fn test_can_serialise_pickled_session_with_sender_data() {
        // Given an InboundGroupSession
        let igs = InboundGroupSession::new(
            Curve25519PublicKey::from_base64("AmM1DvVJarsNNXVuX7OarzfT481N37GtDwvDVF0RcR8")
                .unwrap(),
            Ed25519PublicKey::from_base64("wTRTdz4rn4EY+68cKPzpMdQ6RAlg7T8cbTmEjaXuUww").unwrap(),
            room_id!("!test:localhost"),
            &create_session_key(),
            SenderData::unknown(),
            None,
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            Some(HistoryVisibility::Shared),
            false,
        )
        .unwrap();

        // When we pickle it
        let pickled = igs.pickle().await;

        // And serialise it
        let serialised = serde_json::to_string(&pickled).unwrap();

        // Then it looks as we expect

        // (Break out this list of numbers as otherwise it bothers the json macro below)
        let expected_inner = vec![
            193, 203, 223, 152, 33, 132, 200, 168, 24, 197, 79, 174, 231, 202, 45, 245, 128, 131,
            178, 165, 148, 37, 241, 214, 178, 218, 25, 33, 68, 48, 153, 104, 122, 6, 249, 198, 97,
            226, 214, 75, 64, 128, 25, 138, 98, 90, 138, 93, 52, 206, 174, 3, 84, 149, 101, 140,
            238, 156, 103, 107, 124, 144, 139, 104, 253, 5, 100, 251, 186, 118, 208, 87, 31, 218,
            123, 234, 103, 34, 246, 100, 39, 90, 216, 72, 187, 86, 202, 150, 100, 116, 204, 254,
            10, 154, 216, 133, 61, 250, 75, 100, 195, 63, 138, 22, 17, 13, 156, 123, 195, 132, 111,
            95, 250, 24, 236, 0, 246, 93, 230, 100, 211, 165, 211, 190, 181, 87, 42, 181,
        ];
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&serialised).unwrap(),
            serde_json::json!({
                "pickle":{
                    "initial_ratchet":{
                        "inner": expected_inner,
                        "counter":0
                    },
                    "signing_key":[
                        213,161,95,135,114,153,162,127,217,74,64,2,59,143,93,5,190,157,120,
                        80,89,8,87,129,115,148,104,144,152,186,178,109
                    ],
                    "signing_key_verified":true,
                    "config":{"version":"V1"}
                },
                "sender_key":"AmM1DvVJarsNNXVuX7OarzfT481N37GtDwvDVF0RcR8",
                "signing_key":{"ed25519":"wTRTdz4rn4EY+68cKPzpMdQ6RAlg7T8cbTmEjaXuUww"},
                "sender_data":{
                    "UnknownDevice":{
                        "legacy_session":false
                    }
                },
                "forwarder_data":null,
                "room_id":"!test:localhost",
                "imported":false,
                "backed_up":false,
                "shared_history":false,
                "history_visibility":"shared",
                "algorithm":"m.megolm.v1.aes-sha2"
            })
        );
    }

    #[async_test]
    async fn test_can_deserialise_pickled_session_with_sender_data() {
        // Given the raw JSON for a picked inbound group session (including sender_data)
        let pickle = r#"
        {
            "pickle": {
                "initial_ratchet": {
                    "inner": [ 124, 251, 213, 204, 108, 247, 54, 7, 179, 162, 15, 107, 154, 215,
                               220, 46, 123, 113, 120, 162, 225, 246, 237, 203, 125, 102, 190, 212,
                               229, 195, 136, 185, 26, 31, 77, 140, 144, 181, 152, 177, 46, 105,
                               202, 6, 53, 158, 157, 170, 31, 155, 130, 87, 214, 110, 143, 55, 68,
                               138, 41, 35, 242, 230, 194, 15, 16, 145, 116, 94, 89, 35, 79, 145,
                               245, 117, 204, 173, 166, 178, 49, 131, 143, 61, 61, 15, 211, 167, 17,
                               2, 79, 110, 149, 200, 223, 23, 185, 200, 29, 64, 55, 39, 147, 167,
                               205, 224, 159, 101, 218, 249, 203, 30, 175, 174, 48, 252, 40, 131,
                               52, 135, 91, 57, 211, 96, 105, 58, 55, 68, 250, 24 ],
                    "counter": 0
                },
                "signing_key": [ 93, 185, 171, 61, 173, 100, 51, 9, 157, 180, 214, 39, 131, 80, 118,
                                 130, 199, 232, 163, 197, 45, 23, 227, 100, 151, 59, 19, 102, 38,
                                 149, 43, 38 ],
                "signing_key_verified": true,
                "config": {
                  "version": "V1"
                }
            },
            "sender_key": "AmM1DvVJarsNNXVuX7OarzfT481N37GtDwvDVF0RcR8",
            "signing_key": {
                "ed25519": "wTRTdz4rn4EY+68cKPzpMdQ6RAlg7T8cbTmEjaXuUww"
            },
            "sender_data":{
                "UnknownDevice":{
                    "legacy_session":false
                }
            },
            "room_id": "!test:localhost",
            "forwarding_chains": ["tb6kQKjk+SJl2KnfQ0lKVOZl6gDFMcsb9HcUP9k/4hc"],
            "imported": false,
            "backed_up": false,
            "history_visibility": "shared",
            "algorithm": "m.megolm.v1.aes-sha2"
        }
        "#;

        // When we deserialise it to from JSON
        let deserialized = serde_json::from_str(pickle).unwrap();

        // And unpickle it
        let unpickled = InboundGroupSession::from_pickle(deserialized).unwrap();

        // Then it was parsed correctly
        assert_eq!(unpickled.session_id(), "XbmrPa1kMwmdtNYng1B2gsfoo8UtF+NklzsTZiaVKyY");

        // And we populated the InboundGroupSession's sender_data with the provided
        // values
        assert_let!(
            SenderData::UnknownDevice { legacy_session, owner_check_failed } =
                unpickled.sender_data
        );
        assert!(!legacy_session);
        assert!(!owner_check_failed);
    }

    #[async_test]
    #[allow(deprecated)]
    async fn test_session_comparison() {
        let alice = Account::with_device_id(alice_id(), alice_device_id());
        let room_id = room_id!("!test:localhost");

        let (_, inbound) = alice.create_group_session_pair_with_defaults(room_id).await;

        let worse = InboundGroupSession::from_export(&inbound.export_at_index(10).await).unwrap();
        let mut copy = InboundGroupSession::from_pickle(inbound.pickle().await).unwrap();

        assert_eq!(inbound.compare(&worse).await, SessionOrdering::Better);
        assert_eq!(inbound.compare_ratchet(&worse).await, SessionOrdering::Better);
        assert_eq!(worse.compare(&inbound).await, SessionOrdering::Worse);
        assert_eq!(worse.compare_ratchet(&inbound).await, SessionOrdering::Worse);
        assert_eq!(inbound.compare(&inbound).await, SessionOrdering::Equal);
        assert_eq!(inbound.compare_ratchet(&inbound).await, SessionOrdering::Equal);
        assert_eq!(inbound.compare(&copy).await, SessionOrdering::Equal);
        assert_eq!(inbound.compare_ratchet(&copy).await, SessionOrdering::Equal);

        copy.creator_info.curve25519_key =
            Curve25519PublicKey::from_base64("XbmrPa1kMwmdtNYng1B2gsfoo8UtF+NklzsTZiaVKyY")
                .unwrap();

        assert_eq!(inbound.compare(&copy).await, SessionOrdering::Unconnected);
        assert_eq!(inbound.compare_ratchet(&copy).await, SessionOrdering::Unconnected);
    }

    #[async_test]
    #[allow(deprecated)]
    async fn test_session_comparison_sender_data() {
        let alice = Account::with_device_id(alice_id(), alice_device_id());
        let room_id = room_id!("!test:localhost");

        let (_, mut inbound) = alice.create_group_session_pair_with_defaults(room_id).await;

        let sender_data = SenderData::SenderVerified(KnownSenderData {
            user_id: alice.user_id().into(),
            device_id: Some(alice.device_id().into()),
            master_key: alice.identity_keys().ed25519.into(),
        });

        let mut better = InboundGroupSession::from_pickle(inbound.pickle().await).unwrap();
        better.sender_data = sender_data.clone();

        assert_eq!(inbound.compare(&better).await, SessionOrdering::Worse);
        assert_eq!(better.compare(&inbound).await, SessionOrdering::Better);

        inbound.sender_data = sender_data;
        assert_eq!(better.compare(&inbound).await, SessionOrdering::Equal);
    }

    fn create_session_key() -> SessionKey {
        SessionKey::from_base64(
            "\
            AgAAAADBy9+YIYTIqBjFT67nyi31gIOypZQl8day2hkhRDCZaHoG+cZh4tZLQIAZimJail0\
            0zq4DVJVljO6cZ2t8kIto/QVk+7p20Fcf2nvqZyL2ZCda2Ei7VsqWZHTM/gqa2IU9+ktkwz\
            +KFhENnHvDhG9f+hjsAPZd5mTTpdO+tVcqtdWhX4dymaJ/2UpAAjuPXQW+nXhQWQhXgXOUa\
            JCYurJtvbCbqZGeDMmVIoqukBs2KugNJ6j5WlTPoeFnMl6Guy9uH2iWWxGg8ZgT2xspqVl5\
            CwujjC+m7Dh1toVkvu+bAw\
            ",
        )
        .unwrap()
    }

    #[async_test]
    async fn test_shared_history_from_m_room_key_content() {
        let content = json!({
            "algorithm": "m.megolm.v1.aes-sha2",
            "room_id": "!Cuyf34gef24t:localhost",
            "org.matrix.msc3061.shared_history": true,
            "session_id": "ZFD6+OmV7fVCsJ7Gap8UnORH8EnmiAkes8FAvQuCw/I",
            "session_key": "AgAAAADNp1EbxXYOGmJtyX4AkD1bvJvAUyPkbIaKxtnGKjv\
                            SQ3E/4mnuqdM4vsmNzpO1EeWzz1rDkUpYhYE9kP7sJhgLXi\
                            jVv80fMPHfGc49hPdu8A+xnwD4SQiYdFmSWJOIqsxeo/fiH\
                            tino//CDQENtcKuEt0I9s0+Kk4YSH310Szse2RQ+vjple31\
                            QrCexmqfFJzkR/BJ5ogJHrPBQL0LgsPyglIbMTLg7qygIaY\
                            U5Fe2QdKMH7nTZPNIRHh1RaMfHVETAUJBax88EWZBoifk80\
                            gdHUwHSgMk77vCc2a5KHKLDA",
        });

        let sender_key = Curve25519PublicKey::from_bytes([0; 32]);
        let signing_key = Ed25519PublicKey::from_slice(&[0; 32]).expect("");
        let mut content: room_key::MegolmV1AesSha2Content = serde_json::from_value(content)
            .expect("We should be able to deserialize the m.room_key content");

        let session = InboundGroupSession::from_room_key_content(sender_key, signing_key, &content)
            .expect(
                "We should be able to create an inbound group session from the room key content",
            );

        assert!(
            session.shared_history,
            "The shared history flag should be set as it was set in the m.room_key content"
        );

        content.shared_history = false;
        let session = InboundGroupSession::from_room_key_content(sender_key, signing_key, &content)
            .expect(
                "We should be able to create an inbound group session from the room key content",
            );

        assert!(
            !session.shared_history,
            "The shared history flag should not be set as it was not set in the m.room_key content"
        );
    }

    #[async_test]
    async fn test_shared_history_from_exported_room_key() {
        let content = json!({
                "algorithm": "m.megolm.v1.aes-sha2",
                "room_id": "!room:id",
                "sender_key": "FOvlmz18LLI3k/llCpqRoKT90+gFF8YhuL+v1YBXHlw",
                "session_id": "/2K+V777vipCxPZ0gpY9qcpz1DYaXwuMRIu0UEP0Wa0",
                "session_key": "AQAAAAAclzWVMeWBKH+B/WMowa3rb4ma3jEl6n5W4GCs9ue65CruzD3ihX+85pZ9hsV9Bf6fvhjp76WNRajoJYX0UIt7aosjmu0i+H+07hEQ0zqTKpVoSH0ykJ6stAMhdr6Q4uW5crBmdTTBIsqmoWsNJZKKoE2+ldYrZ1lrFeaJbjBIY/9ivle++74qQsT2dIKWPanKc9Q2Gl8LjESLtFBD9Fmt",
                "sender_claimed_keys": {
                    "ed25519": "F4P7f1Z0RjbiZMgHk1xBCG3KC4/Ng9PmxLJ4hQ13sHA"
                },
                "forwarding_curve25519_key_chain": [],
                "org.matrix.msc3061.shared_history": true

        });

        let mut content: ExportedRoomKey = serde_json::from_value(content)
            .expect("We should be able to deserialize the m.room_key content");

        let session = InboundGroupSession::from_export(&content).expect(
            "We should be able to create an inbound group session from the room key export",
        );
        assert!(
            session.shared_history,
            "The shared history flag should be set as it was set in the exported room key"
        );

        content.shared_history = false;

        let session = InboundGroupSession::from_export(&content).expect(
            "We should be able to create an inbound group session from the room key export",
        );
        assert!(
            !session.shared_history,
            "The shared history flag should not be set as it was not set in the exported room key"
        );
    }

    #[async_test]
    async fn test_shared_history_from_backed_up_room_key() {
        let content = json!({
                "algorithm": "m.megolm.v1.aes-sha2",
                "sender_key": "FOvlmz18LLI3k/llCpqRoKT90+gFF8YhuL+v1YBXHlw",
                "session_key": "AQAAAAAclzWVMeWBKH+B/WMowa3rb4ma3jEl6n5W4GCs9ue65CruzD3ihX+85pZ9hsV9Bf6fvhjp76WNRajoJYX0UIt7aosjmu0i+H+07hEQ0zqTKpVoSH0ykJ6stAMhdr6Q4uW5crBmdTTBIsqmoWsNJZKKoE2+ldYrZ1lrFeaJbjBIY/9ivle++74qQsT2dIKWPanKc9Q2Gl8LjESLtFBD9Fmt",
                "sender_claimed_keys": {
                    "ed25519": "F4P7f1Z0RjbiZMgHk1xBCG3KC4/Ng9PmxLJ4hQ13sHA"
                },
                "forwarding_curve25519_key_chain": [],
                "org.matrix.msc3061.shared_history": true

        });

        let session_id = "/2K+V777vipCxPZ0gpY9qcpz1DYaXwuMRIu0UEP0Wa0";
        let room_id = owned_room_id!("!room:id");
        let room_key: BackedUpRoomKey = serde_json::from_value(content)
            .expect("We should be able to deserialize the backed up room key");

        let room_key =
            ExportedRoomKey::from_backed_up_room_key(room_id, session_id.to_owned(), room_key);

        let session = InboundGroupSession::from_export(&room_key).expect(
            "We should be able to create an inbound group session from the room key export",
        );
        assert!(
            session.shared_history,
            "The shared history flag should be set as it was set in the backed up room key"
        );
    }

    #[async_test]
    async fn test_shared_history_in_pickle() {
        let alice = Account::with_device_id(alice_id(), alice_device_id());
        let room_id = room_id!("!test:localhost");

        let (_, mut inbound) = alice.create_group_session_pair_with_defaults(room_id).await;

        inbound.shared_history = true;
        let pickle = inbound.pickle().await;

        assert!(
            pickle.shared_history,
            "The set shared history flag should have been copied to the pickle"
        );

        inbound.shared_history = false;
        let pickle = inbound.pickle().await;

        assert!(
            !pickle.shared_history,
            "The unset shared history flag should have been copied to the pickle"
        );
    }

    #[async_test]
    async fn test_shared_history_in_export() {
        let alice = Account::with_device_id(alice_id(), alice_device_id());
        let room_id = room_id!("!test:localhost");

        let (_, mut inbound) = alice.create_group_session_pair_with_defaults(room_id).await;

        inbound.shared_history = true;
        let export = inbound.export().await;
        assert!(
            export.shared_history,
            "The set shared history flag should have been copied to the room key export"
        );

        inbound.shared_history = false;
        let export = inbound.export().await;
        assert!(
            !export.shared_history,
            "The unset shared history flag should have been copied to the room key export"
        );
    }
}
