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
    convert::TryFrom,
    matches,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use matrix_sdk_common::{
    events::{
        key::verification::{
            accept::{
                AcceptEventContent, AcceptMethod, AcceptToDeviceEventContent,
                SasV1Content as AcceptV1Content, SasV1ContentInit as AcceptV1ContentInit,
            },
            cancel::CancelCode,
            done::{DoneEventContent, DoneToDeviceEventContent},
            key::{KeyEventContent, KeyToDeviceEventContent},
            start::{
                SasV1Content, SasV1ContentInit, StartEventContent, StartMethod,
                StartToDeviceEventContent,
            },
            HashAlgorithm, KeyAgreementProtocol, MessageAuthenticationCode, Relation,
            ShortAuthenticationString, VerificationMethod,
        },
        AnyMessageEventContent, AnyToDeviceEventContent,
    },
    identifiers::{DeviceId, EventId, RoomId, UserId},
    uuid::Uuid,
};
use olm_rs::sas::OlmSas;
use tracing::info;

use super::{
    helpers::{
        calculate_commitment, get_decimal, get_emoji, get_emoji_index, get_mac_content,
        receive_mac_event, SasIds,
    },
    OutgoingContent,
};
use crate::{
    identities::{ReadOnlyDevice, UserIdentities},
    verification::{
        event_enums::{
            AcceptContent, DoneContent, KeyContent, MacContent, OwnedAcceptContent,
            OwnedStartContent, StartContent,
        },
        Cancelled, Done, FlowId,
    },
    ReadOnlyAccount,
};

const KEY_AGREEMENT_PROTOCOLS: &[KeyAgreementProtocol] =
    &[KeyAgreementProtocol::Curve25519HkdfSha256];
const HASHES: &[HashAlgorithm] = &[HashAlgorithm::Sha256];
const MACS: &[MessageAuthenticationCode] = &[MessageAuthenticationCode::HkdfHmacSha256];
const STRINGS: &[ShortAuthenticationString] =
    &[ShortAuthenticationString::Decimal, ShortAuthenticationString::Emoji];

// The max time a SAS flow can take from start to done.
const MAX_AGE: Duration = Duration::from_secs(60 * 5);

// The max time a SAS object will wait for a new event to arrive.
const MAX_EVENT_TIMEOUT: Duration = Duration::from_secs(60);

/// Struct containing the protocols that were agreed to be used for the SAS
/// flow.
#[derive(Clone, Debug)]
pub struct AcceptedProtocols {
    pub method: VerificationMethod,
    pub key_agreement_protocol: KeyAgreementProtocol,
    pub hash: HashAlgorithm,
    pub message_auth_code: MessageAuthenticationCode,
    pub short_auth_string: Vec<ShortAuthenticationString>,
}

impl TryFrom<AcceptV1Content> for AcceptedProtocols {
    type Error = CancelCode;

    fn try_from(content: AcceptV1Content) -> Result<Self, Self::Error> {
        if !KEY_AGREEMENT_PROTOCOLS.contains(&content.key_agreement_protocol)
            || !HASHES.contains(&content.hash)
            || !MACS.contains(&content.message_authentication_code)
            || (!content.short_authentication_string.contains(&ShortAuthenticationString::Emoji)
                && !content
                    .short_authentication_string
                    .contains(&ShortAuthenticationString::Decimal))
        {
            Err(CancelCode::UnknownMethod)
        } else {
            Ok(Self {
                method: VerificationMethod::MSasV1,
                hash: content.hash,
                key_agreement_protocol: content.key_agreement_protocol,
                message_auth_code: content.message_authentication_code,
                short_auth_string: content.short_authentication_string,
            })
        }
    }
}

impl TryFrom<&SasV1Content> for AcceptedProtocols {
    type Error = CancelCode;

    fn try_from(method_content: &SasV1Content) -> Result<Self, Self::Error> {
        if !method_content
            .key_agreement_protocols
            .contains(&KeyAgreementProtocol::Curve25519HkdfSha256)
            || !method_content
                .message_authentication_codes
                .contains(&MessageAuthenticationCode::HkdfHmacSha256)
            || !method_content.hashes.contains(&HashAlgorithm::Sha256)
            || (!method_content
                .short_authentication_string
                .contains(&ShortAuthenticationString::Decimal)
                && !method_content
                    .short_authentication_string
                    .contains(&ShortAuthenticationString::Emoji))
        {
            Err(CancelCode::UnknownMethod)
        } else {
            let mut short_auth_string = vec![];

            if method_content
                .short_authentication_string
                .contains(&ShortAuthenticationString::Decimal)
            {
                short_auth_string.push(ShortAuthenticationString::Decimal)
            }

            if method_content
                .short_authentication_string
                .contains(&ShortAuthenticationString::Emoji)
            {
                short_auth_string.push(ShortAuthenticationString::Emoji);
            }

            Ok(Self {
                method: VerificationMethod::MSasV1,
                hash: HashAlgorithm::Sha256,
                key_agreement_protocol: KeyAgreementProtocol::Curve25519HkdfSha256,
                message_auth_code: MessageAuthenticationCode::HkdfHmacSha256,
                short_auth_string,
            })
        }
    }
}

#[cfg(not(tarpaulin_include))]
impl Default for AcceptedProtocols {
    fn default() -> Self {
        AcceptedProtocols {
            method: VerificationMethod::MSasV1,
            hash: HashAlgorithm::Sha256,
            key_agreement_protocol: KeyAgreementProtocol::Curve25519HkdfSha256,
            message_auth_code: MessageAuthenticationCode::HkdfHmacSha256,
            short_auth_string: vec![
                ShortAuthenticationString::Decimal,
                ShortAuthenticationString::Emoji,
            ],
        }
    }
}

/// A type level state machine modeling the Sas flow.
///
/// This is the generic struc holding common data between the different states
/// and the specific state.
#[derive(Clone)]
pub struct SasState<S: Clone> {
    /// The Olm SAS struct.
    inner: Arc<Mutex<OlmSas>>,

    /// Struct holding the identities that are doing the SAS dance.
    ids: SasIds,

    /// The instant when the SAS object was created. If this more than
    /// MAX_AGE seconds are elapsed, the event will be canceled with a
    /// `CancelCode::Timeout`
    creation_time: Arc<Instant>,

    /// The instant the SAS object last received an event.
    last_event_time: Arc<Instant>,

    /// The unique identifier of this SAS flow.
    ///
    /// This will be the transaction id for to-device events and the relates_to
    /// field for in-room events.
    pub verification_flow_id: Arc<FlowId>,

    /// The SAS state we're in.
    pub state: Arc<S>,

    /// Did the SAS verification start from a `m.verification.request`.
    pub started_from_request: bool,
}

#[cfg(not(tarpaulin_include))]
impl<S: Clone + std::fmt::Debug> std::fmt::Debug for SasState<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SasState")
            .field("ids", &self.ids)
            .field("flow_id", &self.verification_flow_id)
            .field("state", &self.state)
            .finish()
    }
}

/// The initial SAS state.
#[derive(Clone, Debug)]
pub struct Created {
    protocol_definitions: SasV1ContentInit,
}

/// The initial SAS state if the other side started the SAS verification.
#[derive(Clone, Debug)]
pub struct Started {
    commitment: String,
    pub accepted_protocols: Arc<AcceptedProtocols>,
}

/// The SAS state we're going to be in after the other side accepted our
/// verification start event.
#[derive(Clone, Debug)]
pub struct Accepted {
    pub accepted_protocols: Arc<AcceptedProtocols>,
    start_content: Arc<OwnedStartContent>,
    commitment: String,
}

/// The SAS state we're going to be in after we received the public key of the
/// other participant.
///
/// From now on we can show the short auth string to the user.
#[derive(Clone, Debug)]
pub struct KeyReceived {
    their_pubkey: String,
    we_started: bool,
    pub accepted_protocols: Arc<AcceptedProtocols>,
}

/// The SAS state we're going to be in after the user has confirmed that the
/// short auth string matches. We still need to receive a MAC event from the
/// other side.
#[derive(Clone, Debug)]
pub struct Confirmed {
    pub accepted_protocols: Arc<AcceptedProtocols>,
}

/// The SAS state we're going to be in after we receive a MAC event from the
/// other side. Our own user still needs to confirm that the short auth string
/// matches.
#[derive(Clone, Debug)]
pub struct MacReceived {
    we_started: bool,
    their_pubkey: String,
    verified_devices: Arc<[ReadOnlyDevice]>,
    verified_master_keys: Arc<[UserIdentities]>,
    pub accepted_protocols: Arc<AcceptedProtocols>,
}

/// The SAS state we're going to be in after we receive a MAC event in a DM. DMs
/// require a final message `m.key.verification.done` message to conclude the
/// verification. This state waits for such a message.
#[derive(Clone, Debug)]
pub struct WaitingForDone {
    verified_devices: Arc<[ReadOnlyDevice]>,
    verified_master_keys: Arc<[UserIdentities]>,
}

impl<S: Clone> SasState<S> {
    /// Get our own user id.
    #[cfg(test)]
    pub fn user_id(&self) -> &UserId {
        &self.ids.account.user_id()
    }

    /// Get our own device id.
    pub fn device_id(&self) -> &DeviceId {
        &self.ids.account.device_id()
    }

    #[cfg(test)]
    pub fn other_device(&self) -> ReadOnlyDevice {
        self.ids.other_device.clone()
    }

    pub fn cancel(self, cancel_code: CancelCode) -> SasState<Cancelled> {
        SasState {
            inner: self.inner,
            ids: self.ids,
            creation_time: self.creation_time,
            last_event_time: self.last_event_time,
            verification_flow_id: self.verification_flow_id,
            state: Arc::new(Cancelled::new(cancel_code)),
            started_from_request: self.started_from_request,
        }
    }

    /// Did our SAS verification time out.
    pub fn timed_out(&self) -> bool {
        self.creation_time.elapsed() > MAX_AGE || self.last_event_time.elapsed() > MAX_EVENT_TIMEOUT
    }

    /// Is this verification happening inside a DM.
    #[allow(dead_code)]
    pub fn is_dm_verification(&self) -> bool {
        matches!(&*self.verification_flow_id, FlowId::InRoom(_, _))
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn set_creation_time(&mut self, time: Instant) {
        self.creation_time = Arc::new(time);
    }

    fn check_event(&self, sender: &UserId, flow_id: &str) -> Result<(), CancelCode> {
        if *flow_id != *self.verification_flow_id.as_str() {
            Err(CancelCode::UnknownTransaction)
        } else if sender != self.ids.other_device.user_id() {
            Err(CancelCode::UserMismatch)
        } else if self.timed_out() {
            Err(CancelCode::Timeout)
        } else {
            Ok(())
        }
    }
}

impl SasState<Created> {
    /// Create a new SAS verification flow.
    ///
    /// # Arguments
    ///
    /// * `account` - Our own account.
    ///
    /// * `other_device` - The other device which we are going to verify.
    ///
    /// * `other_identity` - The identity of the other user if one exists.
    pub fn new(
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
        transaction_id: Option<String>,
    ) -> SasState<Created> {
        let started_from_request = transaction_id.is_some();
        let flow_id =
            FlowId::ToDevice(transaction_id.unwrap_or_else(|| Uuid::new_v4().to_string()));
        Self::new_helper(flow_id, account, other_device, other_identity, started_from_request)
    }

    /// Create a new SAS in-room verification flow.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The event id of the `m.key.verification.request` event
    /// that started the verification flow.
    ///
    /// * `account` - Our own account.
    ///
    /// * `other_device` - The other device which we are going to verify.
    ///
    /// * `other_identity` - The identity of the other user if one exists.
    pub fn new_in_room(
        room_id: RoomId,
        event_id: EventId,
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
    ) -> SasState<Created> {
        let flow_id = FlowId::InRoom(room_id, event_id);
        Self::new_helper(flow_id, account, other_device, other_identity, false)
    }

    fn new_helper(
        flow_id: FlowId,
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
        started_from_request: bool,
    ) -> SasState<Created> {
        SasState {
            inner: Arc::new(Mutex::new(OlmSas::new())),
            ids: SasIds { account, other_device, other_identity },
            verification_flow_id: flow_id.into(),

            creation_time: Arc::new(Instant::now()),
            last_event_time: Arc::new(Instant::now()),
            started_from_request,

            state: Arc::new(Created {
                protocol_definitions: SasV1ContentInit {
                    short_authentication_string: STRINGS.to_vec(),
                    key_agreement_protocols: KEY_AGREEMENT_PROTOCOLS.to_vec(),
                    message_authentication_codes: MACS.to_vec(),
                    hashes: HASHES.to_vec(),
                },
            }),
        }
    }

    pub fn as_content(&self) -> OwnedStartContent {
        match self.verification_flow_id.as_ref() {
            FlowId::ToDevice(s) => OwnedStartContent::ToDevice(StartToDeviceEventContent::new(
                self.device_id().into(),
                s.to_string(),
                StartMethod::SasV1(
                    SasV1Content::new(self.state.protocol_definitions.clone())
                        .expect("Invalid initial protocol definitions."),
                ),
            )),
            FlowId::InRoom(r, e) => OwnedStartContent::Room(
                r.clone(),
                StartEventContent::new(
                    self.device_id().into(),
                    StartMethod::SasV1(
                        SasV1Content::new(self.state.protocol_definitions.clone())
                            .expect("Invalid initial protocol definitions."),
                    ),
                    Relation::new(e.clone()),
                ),
            ),
        }
    }

    /// Receive a m.key.verification.accept event, changing the state into
    /// an Accepted one.
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.accept event that was sent to us by
    /// the other side.
    pub fn into_accepted(
        self,
        sender: &UserId,
        content: &AcceptContent,
    ) -> Result<SasState<Accepted>, SasState<Cancelled>> {
        self.check_event(&sender, content.flow_id()).map_err(|c| self.clone().cancel(c))?;

        if let AcceptMethod::MSasV1(content) = content.method() {
            let accepted_protocols =
                AcceptedProtocols::try_from(content.clone()).map_err(|c| self.clone().cancel(c))?;

            let start_content = self.as_content().into();

            Ok(SasState {
                inner: self.inner,
                ids: self.ids,
                verification_flow_id: self.verification_flow_id,
                creation_time: self.creation_time,
                last_event_time: self.last_event_time,
                started_from_request: self.started_from_request,
                state: Arc::new(Accepted {
                    start_content,
                    commitment: content.commitment.clone(),
                    accepted_protocols: accepted_protocols.into(),
                }),
            })
        } else {
            Err(self.cancel(CancelCode::UnknownMethod))
        }
    }
}

impl SasState<Started> {
    /// Create a new SAS verification flow from an in-room
    /// m.key.verification.start event.
    ///
    /// This will put us in the `started` state.
    ///
    /// # Arguments
    ///
    /// * `account` - Our own account.
    ///
    /// * `other_device` - The other device which we are going to verify.
    ///
    /// * `event` - The m.key.verification.start event that was sent to us by
    /// the other side.
    pub fn from_start_event(
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
        flow_id: FlowId,
        content: &StartContent,
        started_from_request: bool,
    ) -> Result<SasState<Started>, SasState<Cancelled>> {
        let flow_id = Arc::new(flow_id);

        let canceled = || SasState {
            inner: Arc::new(Mutex::new(OlmSas::new())),

            creation_time: Arc::new(Instant::now()),
            last_event_time: Arc::new(Instant::now()),
            started_from_request,

            ids: SasIds {
                account: account.clone(),
                other_device: other_device.clone(),
                other_identity: other_identity.clone(),
            },

            verification_flow_id: flow_id.clone(),
            state: Arc::new(Cancelled::new(CancelCode::UnknownMethod)),
        };

        if let StartMethod::SasV1(method_content) = content.method() {
            let sas = OlmSas::new();

            let pubkey = sas.public_key();
            let commitment = calculate_commitment(&pubkey, content);

            info!(
                "Calculated commitment for pubkey {} and content {:?} {}",
                pubkey, content, commitment
            );

            if let Ok(accepted_protocols) = AcceptedProtocols::try_from(method_content) {
                Ok(SasState {
                    inner: Arc::new(Mutex::new(sas)),

                    ids: SasIds { account, other_device, other_identity },

                    creation_time: Arc::new(Instant::now()),
                    last_event_time: Arc::new(Instant::now()),
                    started_from_request,

                    verification_flow_id: flow_id,

                    state: Arc::new(Started {
                        accepted_protocols: accepted_protocols.into(),
                        commitment,
                    }),
                })
            } else {
                Err(canceled())
            }
        } else {
            Err(canceled())
        }
    }

    /// Get the content for the accept event.
    ///
    /// The content needs to be sent to the other device.
    ///
    /// This should be sent out automatically if the SAS verification flow has
    /// been started because of a
    /// m.key.verification.request -> m.key.verification.ready flow.
    pub fn as_content(&self) -> OwnedAcceptContent {
        let method = AcceptMethod::MSasV1(
            AcceptV1ContentInit {
                commitment: self.state.commitment.clone(),
                hash: self.state.accepted_protocols.hash.clone(),
                key_agreement_protocol: self
                    .state
                    .accepted_protocols
                    .key_agreement_protocol
                    .clone(),
                message_authentication_code: self
                    .state
                    .accepted_protocols
                    .message_auth_code
                    .clone(),
                short_authentication_string: self
                    .state
                    .accepted_protocols
                    .short_auth_string
                    .clone(),
            }
            .into(),
        );

        match self.verification_flow_id.as_ref() {
            FlowId::ToDevice(s) => AcceptToDeviceEventContent::new(s.to_string(), method).into(),
            FlowId::InRoom(r, e) => {
                (r.clone(), AcceptEventContent::new(method, Relation::new(e.clone()))).into()
            }
        }
    }

    /// Receive a m.key.verification.key event, changing the state into
    /// a `KeyReceived` one
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.key event that was sent to us by
    /// the other side. The event will be modified so it doesn't contain any key
    /// anymore.
    pub fn into_key_received(
        self,
        sender: &UserId,
        content: &KeyContent,
    ) -> Result<SasState<KeyReceived>, SasState<Cancelled>> {
        self.check_event(&sender, &content.flow_id()).map_err(|c| self.clone().cancel(c))?;

        let their_pubkey = content.public_key().to_owned();

        self.inner
            .lock()
            .unwrap()
            .set_their_public_key(their_pubkey.clone())
            .expect("Can't set public key");

        Ok(SasState {
            inner: self.inner,
            ids: self.ids,
            verification_flow_id: self.verification_flow_id,
            creation_time: self.creation_time,
            last_event_time: self.last_event_time,
            started_from_request: self.started_from_request,
            state: Arc::new(KeyReceived {
                we_started: false,
                their_pubkey,
                accepted_protocols: self.state.accepted_protocols.clone(),
            }),
        })
    }
}

impl SasState<Accepted> {
    /// Receive a m.key.verification.key event, changing the state into
    /// a `KeyReceived` one
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.key event that was sent to us by
    /// the other side. The event will be modified so it doesn't contain any key
    /// anymore.
    pub fn into_key_received(
        self,
        sender: &UserId,
        content: &KeyContent,
    ) -> Result<SasState<KeyReceived>, SasState<Cancelled>> {
        self.check_event(&sender, content.flow_id()).map_err(|c| self.clone().cancel(c))?;

        let commitment = calculate_commitment(
            content.public_key(),
            &self.state.start_content.as_start_content(),
        );

        if self.state.commitment != commitment {
            Err(self.cancel(CancelCode::InvalidMessage))
        } else {
            let their_pubkey = content.public_key().to_owned();

            self.inner
                .lock()
                .unwrap()
                .set_their_public_key(their_pubkey.clone())
                .expect("Can't set public key");

            Ok(SasState {
                inner: self.inner,
                ids: self.ids,
                verification_flow_id: self.verification_flow_id,
                creation_time: self.creation_time,
                last_event_time: self.last_event_time,
                started_from_request: self.started_from_request,
                state: Arc::new(KeyReceived {
                    their_pubkey,
                    we_started: true,
                    accepted_protocols: self.state.accepted_protocols.clone(),
                }),
            })
        }
    }

    /// Get the content for the key event.
    ///
    /// The content needs to be automatically sent to the other side.
    pub fn as_content(&self) -> OutgoingContent {
        match &*self.verification_flow_id {
            FlowId::ToDevice(s) => {
                AnyToDeviceEventContent::KeyVerificationKey(KeyToDeviceEventContent {
                    transaction_id: s.to_string(),
                    key: self.inner.lock().unwrap().public_key(),
                })
                .into()
            }
            FlowId::InRoom(r, e) => (
                r.clone(),
                AnyMessageEventContent::KeyVerificationKey(KeyEventContent::new(
                    self.inner.lock().unwrap().public_key(),
                    Relation::new(e.clone()),
                )),
            )
                .into(),
        }
    }
}

impl SasState<KeyReceived> {
    /// Get the content for the key event.
    ///
    /// The content needs to be automatically sent to the other side if and only
    /// if we_started is false.
    pub fn as_content(&self) -> OutgoingContent {
        match &*self.verification_flow_id {
            FlowId::ToDevice(s) => {
                AnyToDeviceEventContent::KeyVerificationKey(KeyToDeviceEventContent {
                    transaction_id: s.to_string(),
                    key: self.inner.lock().unwrap().public_key(),
                })
                .into()
            }
            FlowId::InRoom(r, e) => (
                r.clone(),
                AnyMessageEventContent::KeyVerificationKey(KeyEventContent::new(
                    self.inner.lock().unwrap().public_key(),
                    Relation::new(e.clone()),
                )),
            )
                .into(),
        }
    }

    /// Get the emoji version of the short authentication string.
    ///
    /// Returns a seven tuples where the first element is the emoji and the
    /// second element the English description of the emoji.
    pub fn get_emoji(&self) -> [(&'static str, &'static str); 7] {
        get_emoji(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.state.their_pubkey,
            self.verification_flow_id.as_str(),
            self.state.we_started,
        )
    }

    /// Get the index of the emoji of the short authentication string.
    ///
    /// Returns seven u8 numbers in the range from 0 to 63 inclusive, those
    /// numbers can be converted to a unique emoji defined by the spec.
    pub fn get_emoji_index(&self) -> [u8; 7] {
        get_emoji_index(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.state.their_pubkey,
            self.verification_flow_id.as_str(),
            self.state.we_started,
        )
    }

    /// Get the decimal version of the short authentication string.
    ///
    /// Returns a tuple containing three 4 digit integer numbers that represent
    /// the short auth string.
    pub fn get_decimal(&self) -> (u16, u16, u16) {
        get_decimal(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.state.their_pubkey,
            self.verification_flow_id.as_str(),
            self.state.we_started,
        )
    }

    /// Receive a m.key.verification.mac event, changing the state into
    /// a `MacReceived` one
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.mac event that was sent to us by
    /// the other side.
    pub fn into_mac_received(
        self,
        sender: &UserId,
        content: &MacContent,
    ) -> Result<SasState<MacReceived>, SasState<Cancelled>> {
        self.check_event(&sender, content.flow_id()).map_err(|c| self.clone().cancel(c))?;

        let (devices, master_keys) = receive_mac_event(
            &self.inner.lock().unwrap(),
            &self.ids,
            self.verification_flow_id.as_str(),
            sender,
            &content,
        )
        .map_err(|c| self.clone().cancel(c))?;

        Ok(SasState {
            inner: self.inner,
            verification_flow_id: self.verification_flow_id,
            creation_time: self.creation_time,
            last_event_time: self.last_event_time,
            ids: self.ids,
            started_from_request: self.started_from_request,
            state: Arc::new(MacReceived {
                we_started: self.state.we_started,
                their_pubkey: self.state.their_pubkey.clone(),
                verified_devices: devices.into(),
                verified_master_keys: master_keys.into(),
                accepted_protocols: self.state.accepted_protocols.clone(),
            }),
        })
    }

    /// Confirm that the short auth string matches.
    ///
    /// This needs to be done by the user, this will put us in the `Confirmed`
    /// state.
    pub fn confirm(self) -> SasState<Confirmed> {
        SasState {
            inner: self.inner,
            started_from_request: self.started_from_request,
            verification_flow_id: self.verification_flow_id,
            creation_time: self.creation_time,
            last_event_time: self.last_event_time,
            ids: self.ids,
            state: Arc::new(Confirmed {
                accepted_protocols: self.state.accepted_protocols.clone(),
            }),
        }
    }
}

impl SasState<Confirmed> {
    /// Receive a m.key.verification.mac event, changing the state into
    /// a `Done` one
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.mac event that was sent to us by
    /// the other side.
    pub fn into_done(
        self,
        sender: &UserId,
        content: &MacContent,
    ) -> Result<SasState<Done>, SasState<Cancelled>> {
        self.check_event(sender, content.flow_id()).map_err(|c| self.clone().cancel(c))?;

        let (devices, master_keys) = receive_mac_event(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.verification_flow_id.as_str(),
            sender,
            &content,
        )
        .map_err(|c| self.clone().cancel(c))?;

        Ok(SasState {
            inner: self.inner,
            creation_time: self.creation_time,
            last_event_time: self.last_event_time,
            verification_flow_id: self.verification_flow_id,
            started_from_request: self.started_from_request,
            ids: self.ids,

            state: Arc::new(Done {
                verified_devices: devices.into(),
                verified_master_keys: master_keys.into(),
            }),
        })
    }

    /// Receive a m.key.verification.mac event, changing the state into
    /// a `WaitingForDone` one. This method should be used instead of
    /// `into_done()` if the verification started with a
    /// `m.key.verification.request`.
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.mac event that was sent to us by
    /// the other side.
    pub fn into_waiting_for_done(
        self,
        sender: &UserId,
        content: &MacContent,
    ) -> Result<SasState<WaitingForDone>, SasState<Cancelled>> {
        self.check_event(&sender, &content.flow_id()).map_err(|c| self.clone().cancel(c))?;

        let (devices, master_keys) = receive_mac_event(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.verification_flow_id.as_str(),
            sender,
            &content,
        )
        .map_err(|c| self.clone().cancel(c))?;

        Ok(SasState {
            inner: self.inner,
            creation_time: self.creation_time,
            last_event_time: self.last_event_time,
            verification_flow_id: self.verification_flow_id,
            started_from_request: self.started_from_request,
            ids: self.ids,

            state: Arc::new(WaitingForDone {
                verified_devices: devices.into(),
                verified_master_keys: master_keys.into(),
            }),
        })
    }

    /// Get the content for the mac event.
    ///
    /// The content needs to be automatically sent to the other side.
    pub fn as_content(&self) -> OutgoingContent {
        get_mac_content(&self.inner.lock().unwrap(), &self.ids, &self.verification_flow_id)
    }
}

impl SasState<MacReceived> {
    /// Confirm that the short auth string matches.
    ///
    /// This needs to be done by the user, this will put us in the `Done`
    /// state since the other side already confirmed and sent us a MAC event.
    pub fn confirm(self) -> SasState<Done> {
        SasState {
            inner: self.inner,
            verification_flow_id: self.verification_flow_id,
            creation_time: self.creation_time,
            started_from_request: self.started_from_request,
            last_event_time: self.last_event_time,
            ids: self.ids,
            state: Arc::new(Done {
                verified_devices: self.state.verified_devices.clone(),
                verified_master_keys: self.state.verified_master_keys.clone(),
            }),
        }
    }

    /// Confirm that the short auth string matches but wait for the other side
    /// to confirm that it's done.
    ///
    /// This needs to be done by the user, this will put us in the `WaitForDone`
    /// state where we wait for the other side to confirm that the MAC event was
    /// successfully received.
    pub fn confirm_and_wait_for_done(self) -> SasState<WaitingForDone> {
        SasState {
            inner: self.inner,
            verification_flow_id: self.verification_flow_id,
            creation_time: self.creation_time,
            started_from_request: self.started_from_request,
            last_event_time: self.last_event_time,
            ids: self.ids,
            state: Arc::new(WaitingForDone {
                verified_devices: self.state.verified_devices.clone(),
                verified_master_keys: self.state.verified_master_keys.clone(),
            }),
        }
    }

    /// Get the emoji version of the short authentication string.
    ///
    /// Returns a vector of tuples where the first element is the emoji and the
    /// second element the English description of the emoji.
    pub fn get_emoji(&self) -> [(&'static str, &'static str); 7] {
        get_emoji(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.state.their_pubkey,
            &self.verification_flow_id.as_str(),
            self.state.we_started,
        )
    }

    /// Get the index of the emoji of the short authentication string.
    ///
    /// Returns seven u8 numbers in the range from 0 to 63 inclusive, those
    /// numbers can be converted to a unique emoji defined by the spec.
    pub fn get_emoji_index(&self) -> [u8; 7] {
        get_emoji_index(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.state.their_pubkey,
            self.verification_flow_id.as_str(),
            self.state.we_started,
        )
    }

    /// Get the decimal version of the short authentication string.
    ///
    /// Returns a tuple containing three 4 digit integer numbers that represent
    /// the short auth string.
    pub fn get_decimal(&self) -> (u16, u16, u16) {
        get_decimal(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.state.their_pubkey,
            &self.verification_flow_id.as_str(),
            self.state.we_started,
        )
    }
}

impl SasState<WaitingForDone> {
    /// Get the content for the mac event.
    ///
    /// The content needs to be automatically sent to the other side if it
    /// wasn't already sent.
    pub fn as_content(&self) -> OutgoingContent {
        get_mac_content(&self.inner.lock().unwrap(), &self.ids, &self.verification_flow_id)
    }

    pub fn done_content(&self) -> OutgoingContent {
        match self.verification_flow_id.as_ref() {
            FlowId::ToDevice(t) => AnyToDeviceEventContent::KeyVerificationDone(
                DoneToDeviceEventContent::new(t.to_owned()),
            )
            .into(),
            FlowId::InRoom(r, e) => (
                r.clone(),
                AnyMessageEventContent::KeyVerificationDone(DoneEventContent::new(Relation::new(
                    e.clone(),
                ))),
            )
                .into(),
        }
    }

    /// Receive a m.key.verification.mac event, changing the state into
    /// a `Done` one
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.mac event that was sent to us by
    /// the other side.
    pub fn into_done(
        self,
        sender: &UserId,
        content: &DoneContent,
    ) -> Result<SasState<Done>, SasState<Cancelled>> {
        self.check_event(&sender, content.flow_id()).map_err(|c| self.clone().cancel(c))?;

        Ok(SasState {
            inner: self.inner,
            creation_time: self.creation_time,
            last_event_time: self.last_event_time,
            verification_flow_id: self.verification_flow_id,
            started_from_request: self.started_from_request,
            ids: self.ids,

            state: Arc::new(Done {
                verified_devices: self.state.verified_devices.clone(),
                verified_master_keys: self.state.verified_master_keys.clone(),
            }),
        })
    }
}

impl SasState<Done> {
    /// Get the content for the mac event.
    ///
    /// The content needs to be automatically sent to the other side if it
    /// wasn't already sent.
    pub fn as_content(&self) -> OutgoingContent {
        get_mac_content(&self.inner.lock().unwrap(), &self.ids, &self.verification_flow_id)
    }

    pub fn done_content(&self) -> OutgoingContent {
        self.state.as_content(self.verification_flow_id.as_ref())
    }

    /// Get the list of verified devices.
    pub fn verified_devices(&self) -> Arc<[ReadOnlyDevice]> {
        self.state.verified_devices.clone()
    }

    /// Get the list of verified identities.
    pub fn verified_identities(&self) -> Arc<[UserIdentities]> {
        self.state.verified_master_keys.clone()
    }
}

impl SasState<Cancelled> {
    pub fn as_content(&self) -> OutgoingContent {
        self.state.as_content(&self.verification_flow_id)
    }
}

#[cfg(test)]
mod test {
    use std::convert::TryFrom;

    use matrix_sdk_common::{
        events::key::verification::{
            accept::{AcceptMethod, CustomContent},
            start::{CustomContent as CustomStartContent, StartMethod},
        },
        identifiers::{DeviceId, UserId},
    };

    use super::{Accepted, Created, SasState, Started};
    use crate::{
        verification::event_enums::{AcceptContent, KeyContent, MacContent, StartContent},
        ReadOnlyAccount, ReadOnlyDevice,
    };

    fn alice_id() -> UserId {
        UserId::try_from("@alice:example.org").unwrap()
    }

    fn alice_device_id() -> Box<DeviceId> {
        "JLAFKJWSCS".into()
    }

    fn bob_id() -> UserId {
        UserId::try_from("@bob:example.org").unwrap()
    }

    fn bob_device_id() -> Box<DeviceId> {
        "BOBDEVCIE".into()
    }

    async fn get_sas_pair() -> (SasState<Created>, SasState<Started>) {
        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let alice_device = ReadOnlyDevice::from_account(&alice).await;

        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());
        let bob_device = ReadOnlyDevice::from_account(&bob).await;

        let alice_sas = SasState::<Created>::new(alice.clone(), bob_device, None, None);

        let start_content = alice_sas.as_content();
        let flow_id = start_content.flow_id();

        let bob_sas = SasState::<Started>::from_start_event(
            bob.clone(),
            alice_device,
            None,
            flow_id,
            &start_content.as_start_content(),
            false,
        );

        (alice_sas, bob_sas.unwrap())
    }

    #[tokio::test]
    async fn create_sas() {
        let (_, _) = get_sas_pair().await;
    }

    #[tokio::test]
    async fn sas_accept() {
        let (alice, bob) = get_sas_pair().await;
        let content = bob.as_content();
        let content = AcceptContent::from(&content);

        alice.into_accepted(bob.user_id(), &content).unwrap();
    }

    #[tokio::test]
    async fn sas_key_share() {
        let (alice, bob) = get_sas_pair().await;

        let content = bob.as_content();
        let content = AcceptContent::from(&content);

        let alice: SasState<Accepted> = alice.into_accepted(bob.user_id(), &content).unwrap();
        let content = alice.as_content();
        let content = KeyContent::try_from(&content).unwrap();

        let bob = bob.into_key_received(alice.user_id(), &content).unwrap();

        let content = bob.as_content();
        let content = KeyContent::try_from(&content).unwrap();

        let alice = alice.into_key_received(bob.user_id(), &content).unwrap();

        assert_eq!(alice.get_decimal(), bob.get_decimal());
        assert_eq!(alice.get_emoji(), bob.get_emoji());
    }

    #[tokio::test]
    async fn sas_full() {
        let (alice, bob) = get_sas_pair().await;

        let content = bob.as_content();
        let content = AcceptContent::from(&content);

        let alice: SasState<Accepted> = alice.into_accepted(bob.user_id(), &content).unwrap();
        let content = alice.as_content();
        let content = KeyContent::try_from(&content).unwrap();

        let bob = bob.into_key_received(alice.user_id(), &content).unwrap();

        let content = bob.as_content();
        let content = KeyContent::try_from(&content).unwrap();

        let alice = alice.into_key_received(bob.user_id(), &content).unwrap();

        assert_eq!(alice.get_decimal(), bob.get_decimal());
        assert_eq!(alice.get_emoji(), bob.get_emoji());

        let bob_decimals = bob.get_decimal();

        let bob = bob.confirm();

        let content = bob.as_content();
        let content = MacContent::try_from(&content).unwrap();

        let alice = alice.into_mac_received(bob.user_id(), &content).unwrap();
        assert!(!alice.get_emoji().is_empty());
        assert_eq!(alice.get_decimal(), bob_decimals);
        let alice = alice.confirm();

        let content = alice.as_content();
        let content = MacContent::try_from(&content).unwrap();
        let bob = bob.into_done(alice.user_id(), &content).unwrap();

        assert!(bob.verified_devices().contains(&bob.other_device()));
        assert!(alice.verified_devices().contains(&alice.other_device()));
    }

    #[tokio::test]
    async fn sas_invalid_commitment() {
        let (alice, bob) = get_sas_pair().await;

        let mut content = bob.as_content();
        let mut method = content.method_mut();

        match &mut method {
            AcceptMethod::MSasV1(ref mut c) => {
                c.commitment = "".to_string();
            }
            _ => panic!("Unknown accept event content"),
        }

        let content = AcceptContent::from(&content);

        let alice: SasState<Accepted> = alice.into_accepted(bob.user_id(), &content).unwrap();

        let content = alice.as_content();
        let content = KeyContent::try_from(&content).unwrap();
        let bob = bob.into_key_received(alice.user_id(), &content).unwrap();
        let content = bob.as_content();
        let content = KeyContent::try_from(&content).unwrap();

        alice
            .into_key_received(bob.user_id(), &content)
            .expect_err("Didn't cancel on invalid commitment");
    }

    #[tokio::test]
    async fn sas_invalid_sender() {
        let (alice, bob) = get_sas_pair().await;

        let content = bob.as_content();
        let content = AcceptContent::from(&content);
        let sender = UserId::try_from("@malory:example.org").unwrap();
        alice.into_accepted(&sender, &content).expect_err("Didn't cancel on a invalid sender");
    }

    #[tokio::test]
    async fn sas_unknown_sas_method() {
        let (alice, bob) = get_sas_pair().await;

        let mut content = bob.as_content();
        let mut method = content.method_mut();

        match &mut method {
            AcceptMethod::MSasV1(ref mut c) => {
                c.short_authentication_string = vec![];
            }
            _ => panic!("Unknown accept event content"),
        }

        let content = AcceptContent::from(&content);

        alice
            .into_accepted(bob.user_id(), &content)
            .expect_err("Didn't cancel on an invalid SAS method");
    }

    #[tokio::test]
    async fn sas_unknown_method() {
        let (alice, bob) = get_sas_pair().await;

        let mut content = bob.as_content();
        let method = content.method_mut();

        *method = AcceptMethod::Custom(CustomContent {
            method: "m.sas.custom".to_string(),
            data: Default::default(),
        });

        let content = AcceptContent::from(&content);

        alice
            .into_accepted(bob.user_id(), &content)
            .expect_err("Didn't cancel on an unknown SAS method");
    }

    #[tokio::test]
    async fn sas_from_start_unknown_method() {
        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let alice_device = ReadOnlyDevice::from_account(&alice).await;

        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());
        let bob_device = ReadOnlyDevice::from_account(&bob).await;

        let alice_sas = SasState::<Created>::new(alice.clone(), bob_device, None, None);

        let mut start_content = alice_sas.as_content();
        let method = start_content.method_mut();

        match method {
            StartMethod::SasV1(ref mut c) => {
                c.message_authentication_codes = vec![];
            }
            _ => panic!("Unknown SAS start method"),
        }

        let flow_id = start_content.flow_id();
        let content = StartContent::from(&start_content);

        SasState::<Started>::from_start_event(
            bob.clone(),
            alice_device.clone(),
            None,
            flow_id,
            &content,
            false,
        )
        .expect_err("Didn't cancel on invalid MAC method");

        let mut start_content = alice_sas.as_content();
        let method = start_content.method_mut();

        *method = StartMethod::Custom(CustomStartContent {
            method: "m.sas.custom".to_string(),
            data: Default::default(),
        });

        let flow_id = start_content.flow_id();
        let content = StartContent::from(&start_content);

        SasState::<Started>::from_start_event(
            bob.clone(),
            alice_device,
            None,
            flow_id,
            &content,
            false,
        )
        .expect_err("Didn't cancel on unknown sas method");
    }
}
