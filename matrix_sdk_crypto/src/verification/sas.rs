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
    mem,
    sync::{Arc, Mutex},
};

use olm_rs::{sas::OlmSas, utility::OlmUtility};

use matrix_sdk_common::{
    events::{
        key::verification::{
            accept::AcceptEventContent,
            cancel::{CancelCode, CancelEventContent},
            key::KeyEventContent,
            mac::MacEventContent,
            start::{MSasV1Content, MSasV1ContentOptions, StartEventContent},
            HashAlgorithm, KeyAgreementProtocol, MessageAuthenticationCode,
            ShortAuthenticationString, VerificationMethod,
        },
        AnyToDeviceEvent, AnyToDeviceEventContent, ToDeviceEvent,
    },
    identifiers::{DeviceId, UserId},
    uuid::Uuid,
};

use super::{get_decimal, get_emoji, get_mac_content, receive_mac_event, SasIds};
use crate::{Account, Device};

#[derive(Clone, Debug)]
/// Short authentication string object.
pub struct Sas {
    inner: Arc<Mutex<InnerSas>>,
    account: Account,
    other_device: Device,
    flow_id: Arc<String>,
}

impl Sas {
    const KEY_AGREEMENT_PROTOCOLS: &'static [KeyAgreementProtocol] =
        &[KeyAgreementProtocol::Curve25519HkdfSha256];
    const HASHES: &'static [HashAlgorithm] = &[HashAlgorithm::Sha256];
    const MACS: &'static [MessageAuthenticationCode] = &[MessageAuthenticationCode::HkdfHmacSha256];
    const STRINGS: &'static [ShortAuthenticationString] = &[
        ShortAuthenticationString::Decimal,
        ShortAuthenticationString::Emoji,
    ];

    /// Get our own user id.
    pub fn user_id(&self) -> &UserId {
        self.account.user_id()
    }

    /// Get our own device id.
    pub fn device_id(&self) -> &DeviceId {
        self.account.device_id()
    }

    /// Get the user id of the other side.
    pub fn other_user_id(&self) -> &UserId {
        self.other_device.user_id()
    }

    /// Get the device id of the other side.
    pub fn other_device_id(&self) -> &DeviceId {
        self.other_device.device_id()
    }

    /// Get the unique ID that identifies this SAS verification flow.
    pub fn flow_id(&self) -> &str {
        &self.flow_id
    }

    /// Start a new SAS auth flow with the given device.
    ///
    /// # Arguments
    ///
    /// * `account` - Our own account.
    ///
    /// * `other_device` - The other device which we are going to verify.
    ///
    /// Returns the new `Sas` object and a `StartEventContent` that needs to be
    /// sent out through the server to the other device.
    pub(crate) fn start(account: Account, other_device: Device) -> (Sas, StartEventContent) {
        let (inner, content) = InnerSas::start(account.clone(), other_device.clone());
        let flow_id = inner.verification_flow_id();

        let sas = Sas {
            inner: Arc::new(Mutex::new(inner)),
            account,
            other_device,
            flow_id,
        };

        (sas, content)
    }

    /// Create a new Sas object from a m.key.verification.start request.
    ///
    /// # Arguments
    ///
    /// * `account` - Our own account.
    ///
    /// * `other_device` - The other device which we are going to verify.
    ///
    /// * `event` - The m.key.verification.start event that was sent to us by
    /// the other side.
    pub(crate) fn from_start_event(
        account: Account,
        other_device: Device,
        event: &ToDeviceEvent<StartEventContent>,
    ) -> Result<Sas, AnyToDeviceEventContent> {
        let inner = InnerSas::from_start_event(account.clone(), other_device.clone(), event)?;
        let flow_id = inner.verification_flow_id();
        Ok(Sas {
            inner: Arc::new(Mutex::new(inner)),
            account,
            other_device,
            flow_id,
        })
    }

    /// Accept the SAS verification.
    ///
    /// This does nothing if the verification was already accepted, otherwise it
    /// returns an `AcceptEventContent` that needs to be sent out.
    pub fn accept(&self) -> Option<AcceptEventContent> {
        self.inner.lock().unwrap().accept()
    }

    /// Confirm the Sas verification.
    ///
    /// This confirms that the short auth strings match on both sides.
    ///
    /// Does nothing if we're not in a state where we can confirm the short auth
    /// string, otherwise returns a `MacEventContent` that needs to be sent to
    /// the server.
    pub fn confirm(&self) -> Option<MacEventContent> {
        let mut guard = self.inner.lock().unwrap();
        let sas: InnerSas = (*guard).clone();
        let (sas, content) = sas.confirm();
        *guard = sas;
        content
    }

    /// Are we in a state where we can show the short auth string.
    pub fn can_be_presented(&self) -> bool {
        self.inner.lock().unwrap().can_be_presented()
    }

    /// Is the SAS flow done.
    pub fn is_done(&self) -> bool {
        self.inner.lock().unwrap().is_done()
    }

    /// Get the emoji version of the short auth string.
    ///
    /// Returns None if we can't yet present the short auth string, otherwise a
    /// Vec of tuples with the emoji and description.
    pub fn emoji(&self) -> Option<Vec<(&'static str, &'static str)>> {
        self.inner.lock().unwrap().emoji()
    }

    /// Get the decimal version of the short auth string.
    ///
    /// Returns None if we can't yet present the short auth string, otherwise a
    /// tuple containing three 4-digit integers that represent the short auth
    /// string.
    pub fn decimals(&self) -> Option<(u32, u32, u32)> {
        self.inner.lock().unwrap().decimals()
    }

    pub(crate) fn receive_event(
        &self,
        event: &mut AnyToDeviceEvent,
    ) -> Option<AnyToDeviceEventContent> {
        let mut guard = self.inner.lock().unwrap();
        let sas: InnerSas = (*guard).clone();
        let (sas, content) = sas.receive_event(event);
        *guard = sas;

        content
    }

    pub(crate) fn verified_devices(&self) -> Option<Arc<Vec<Box<DeviceId>>>> {
        self.inner.lock().unwrap().verified_devices()
    }
}

#[derive(Clone, Debug)]
enum InnerSas {
    Created(SasState<Created>),
    Started(SasState<Started>),
    Accepted(SasState<Accepted>),
    KeyRecieved(SasState<KeyReceived>),
    Confirmed(SasState<Confirmed>),
    MacReceived(SasState<MacReceived>),
    Done(SasState<Done>),
    Canceled(SasState<Canceled>),
}

impl InnerSas {
    fn start(account: Account, other_device: Device) -> (InnerSas, StartEventContent) {
        let sas = SasState::<Created>::new(account, other_device);
        let content = sas.as_content();
        (InnerSas::Created(sas), content)
    }

    fn from_start_event(
        account: Account,
        other_device: Device,
        event: &ToDeviceEvent<StartEventContent>,
    ) -> Result<InnerSas, AnyToDeviceEventContent> {
        match SasState::<Started>::from_start_event(account, other_device, event) {
            Ok(s) => Ok(InnerSas::Started(s)),
            Err(s) => Err(s.as_content()),
        }
    }

    fn accept(&self) -> Option<AcceptEventContent> {
        if let InnerSas::Started(s) = self {
            Some(s.as_content())
        } else {
            None
        }
    }

    fn confirm(self) -> (InnerSas, Option<MacEventContent>) {
        match self {
            InnerSas::KeyRecieved(s) => {
                let sas = s.confirm();
                let content = sas.as_content();
                (InnerSas::Confirmed(sas), Some(content))
            }
            InnerSas::MacReceived(s) => {
                let sas = s.confirm();
                let content = sas.as_content();
                (InnerSas::Done(sas), Some(content))
            }
            _ => (self, None),
        }
    }

    fn receive_event(
        self,
        event: &mut AnyToDeviceEvent,
    ) -> (InnerSas, Option<AnyToDeviceEventContent>) {
        match event {
            AnyToDeviceEvent::KeyVerificationAccept(e) => {
                if let InnerSas::Created(s) = self {
                    match s.into_accepted(e) {
                        Ok(s) => {
                            let content = s.as_content();
                            (
                                InnerSas::Accepted(s),
                                Some(AnyToDeviceEventContent::KeyVerificationKey(content)),
                            )
                        }
                        Err(s) => {
                            let content = s.as_content();
                            (InnerSas::Canceled(s), Some(content))
                        }
                    }
                } else {
                    (self, None)
                }
            }
            AnyToDeviceEvent::KeyVerificationKey(e) => match self {
                InnerSas::Accepted(s) => match s.into_key_received(e) {
                    Ok(s) => (InnerSas::KeyRecieved(s), None),
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Canceled(s), Some(content))
                    }
                },
                InnerSas::Started(s) => match s.into_key_received(e) {
                    Ok(s) => {
                        let content = s.as_content();
                        (
                            InnerSas::KeyRecieved(s),
                            Some(AnyToDeviceEventContent::KeyVerificationKey(content)),
                        )
                    }
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Canceled(s), Some(content))
                    }
                },
                _ => (self, None),
            },
            AnyToDeviceEvent::KeyVerificationMac(e) => match self {
                InnerSas::KeyRecieved(s) => match s.into_mac_received(e) {
                    Ok(s) => (InnerSas::MacReceived(s), None),
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Canceled(s), Some(content))
                    }
                },
                InnerSas::Confirmed(s) => match s.into_done(e) {
                    Ok(s) => (InnerSas::Done(s), None),
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Canceled(s), Some(content))
                    }
                },
                _ => (self, None),
            },
            _ => (self, None),
        }
    }

    fn can_be_presented(&self) -> bool {
        match self {
            InnerSas::KeyRecieved(_) => true,
            InnerSas::MacReceived(_) => true,
            _ => false,
        }
    }

    fn is_done(&self) -> bool {
        if let InnerSas::Done(_) = self {
            true
        } else {
            false
        }
    }

    fn verification_flow_id(&self) -> Arc<String> {
        match self {
            InnerSas::Created(s) => s.verification_flow_id.clone(),
            InnerSas::Started(s) => s.verification_flow_id.clone(),
            InnerSas::Canceled(s) => s.verification_flow_id.clone(),
            InnerSas::Accepted(s) => s.verification_flow_id.clone(),
            InnerSas::KeyRecieved(s) => s.verification_flow_id.clone(),
            InnerSas::Confirmed(s) => s.verification_flow_id.clone(),
            InnerSas::MacReceived(s) => s.verification_flow_id.clone(),
            InnerSas::Done(s) => s.verification_flow_id.clone(),
        }
    }

    fn emoji(&self) -> Option<Vec<(&'static str, &'static str)>> {
        match self {
            InnerSas::KeyRecieved(s) => Some(s.get_emoji()),
            InnerSas::MacReceived(s) => Some(s.get_emoji()),
            _ => None,
        }
    }

    fn decimals(&self) -> Option<(u32, u32, u32)> {
        match self {
            InnerSas::KeyRecieved(s) => Some(s.get_decimal()),
            InnerSas::MacReceived(s) => Some(s.get_decimal()),
            _ => None,
        }
    }

    fn verified_devices(&self) -> Option<Arc<Vec<Box<DeviceId>>>> {
        if let InnerSas::Done(s) = self {
            Some(s.verified_devices())
        } else {
            None
        }
    }

    fn verified_master_keys(&self) -> Option<Arc<Vec<String>>> {
        if let InnerSas::Done(s) = self {
            Some(s.verified_master_keys())
        } else {
            None
        }
    }
}

/// Struct containing the protocols that were agreed to be used for the SAS
/// flow.
#[derive(Clone, Debug)]
struct AcceptedProtocols {
    method: VerificationMethod,
    key_agreement_protocol: KeyAgreementProtocol,
    hash: HashAlgorithm,
    message_auth_code: MessageAuthenticationCode,
    short_auth_string: Vec<ShortAuthenticationString>,
}

impl From<AcceptEventContent> for AcceptedProtocols {
    fn from(content: AcceptEventContent) -> Self {
        Self {
            method: content.method,
            hash: content.hash,
            key_agreement_protocol: content.key_agreement_protocol,
            message_auth_code: content.message_authentication_code,
            short_auth_string: content.short_authentication_string,
        }
    }
}

/// A type level state machine modeling the Sas flow.
///
/// This is the generic struc holding common data between the different states
/// and the specific state.
#[derive(Clone)]
struct SasState<S: Clone> {
    /// The Olm SAS struct.
    inner: Arc<Mutex<OlmSas>>,
    /// Struct holding the identities that are doing the SAS dance.
    ids: SasIds,
    /// The unique identifier of this SAS flow.
    ///
    /// This will be the transaction id for to-device events and the relates_to
    /// field for in-room events.
    verification_flow_id: Arc<String>,
    /// The SAS state we're in.
    state: Arc<S>,
}

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
struct Created {
    protocol_definitions: MSasV1ContentOptions,
}

/// The initial SAS state if the other side started the SAS verification.
#[derive(Clone, Debug)]
struct Started {
    commitment: String,
    protocol_definitions: MSasV1Content,
}

/// The SAS state we're going to be in after the other side accepted our
/// verification start event.
#[derive(Clone, Debug)]
struct Accepted {
    accepted_protocols: Arc<AcceptedProtocols>,
    json_start_content: String,
    commitment: String,
}

/// The SAS state we're going to be in after we received the public key of the
/// other participant.
///
/// From now on we can show the short auth string to the user.
#[derive(Clone, Debug)]
struct KeyReceived {
    we_started: bool,
    accepted_protocols: Arc<AcceptedProtocols>,
}

/// The SAS state we're going to be in after the user has confirmed that the
/// short auth string matches. We still need to receive a MAC event from the
/// other side.
#[derive(Clone, Debug)]
struct Confirmed {
    accepted_protocols: Arc<AcceptedProtocols>,
}

/// The SAS state we're going to be in after we receive a MAC event from the
/// other side. Our own user still needs to confirm that the short auth string
/// matches.
#[derive(Clone, Debug)]
struct MacReceived {
    we_started: bool,
    verified_devices: Arc<Vec<Box<DeviceId>>>,
    verified_master_keys: Arc<Vec<String>>,
}

/// The SAS state indicating that the verification finished successfully.
///
/// We can now mark the device in our verified devices lits as verified and sign
/// the master keys in the verified devices list.
#[derive(Clone, Debug)]
struct Done {
    verified_devices: Arc<Vec<Box<DeviceId>>>,
    verified_master_keys: Arc<Vec<String>>,
}

#[derive(Clone, Debug)]
struct Canceled {
    cancel_code: CancelCode,
    reason: &'static str,
}

impl<S: Clone> SasState<S> {
    /// Get our own user id.
    fn user_id(&self) -> &UserId {
        &self.ids.account.user_id()
    }

    /// Get our own device id.
    fn device_id(&self) -> &DeviceId {
        &self.ids.account.device_id()
    }

    fn cancel(self, cancel_code: CancelCode) -> SasState<Canceled> {
        SasState {
            inner: self.inner,
            ids: self.ids,
            verification_flow_id: self.verification_flow_id,
            state: Arc::new(Canceled::new(cancel_code)),
        }
    }

    fn check_sender_and_txid(&self, sender: &UserId, flow_id: &str) -> Result<(), CancelCode> {
        if flow_id != *self.verification_flow_id {
            Err(CancelCode::UnknownTransaction)
        } else if sender != self.ids.other_device.user_id() {
            Err(CancelCode::UserMismatch)
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
    fn new(account: Account, other_device: Device) -> SasState<Created> {
        let verification_flow_id = Uuid::new_v4().to_string();
        let from_device: Box<DeviceId> = account.device_id().into();

        SasState {
            inner: Arc::new(Mutex::new(OlmSas::new())),
            ids: SasIds {
                account,
                other_device,
            },
            verification_flow_id: Arc::new(verification_flow_id.clone()),

            state: Arc::new(Created {
                protocol_definitions: MSasV1ContentOptions {
                    transaction_id: verification_flow_id,
                    from_device,
                    short_authentication_string: Sas::STRINGS.to_vec(),
                    key_agreement_protocols: Sas::KEY_AGREEMENT_PROTOCOLS.to_vec(),
                    message_authentication_codes: Sas::MACS.to_vec(),
                    hashes: Sas::HASHES.to_vec(),
                },
            }),
        }
    }

    /// Get the content for the start event.
    ///
    /// The content needs to be sent to the other device.
    fn as_content(&self) -> StartEventContent {
        StartEventContent::MSasV1(
            MSasV1Content::new(self.state.protocol_definitions.clone())
                .expect("Invalid initial protocol definitions."),
        )
    }

    /// Receive a m.key.verification.accept event, changing the state into
    /// an Accepted one.
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.accept event that was sent to us by
    /// the other side.
    fn into_accepted(
        self,
        event: &ToDeviceEvent<AcceptEventContent>,
    ) -> Result<SasState<Accepted>, SasState<Canceled>> {
        self.check_sender_and_txid(&event.sender, &event.content.transaction_id)
            .map_err(|c| self.clone().cancel(c))?;

        let content = &event.content;
        if !Sas::KEY_AGREEMENT_PROTOCOLS.contains(&event.content.key_agreement_protocol)
            || !Sas::HASHES.contains(&event.content.hash)
            || !Sas::MACS.contains(&event.content.message_authentication_code)
            || (!event
                .content
                .short_authentication_string
                .contains(&ShortAuthenticationString::Emoji)
                && !event
                    .content
                    .short_authentication_string
                    .contains(&ShortAuthenticationString::Decimal))
        {
            Err(self.cancel(CancelCode::UnknownMethod))
        } else {
            let json_start_content = cjson::to_string(&self.as_content())
                .expect("Can't deserialize start event content");

            Ok(SasState {
                inner: self.inner,
                ids: self.ids,
                verification_flow_id: self.verification_flow_id,
                state: Arc::new(Accepted {
                    json_start_content,
                    commitment: content.commitment.clone(),
                    accepted_protocols: Arc::new(content.clone().into()),
                }),
            })
        }
    }
}

impl SasState<Started> {
    /// Create a new SAS verification flow from a m.key.verification.start
    /// event.
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
    fn from_start_event(
        account: Account,
        other_device: Device,
        event: &ToDeviceEvent<StartEventContent>,
    ) -> Result<SasState<Started>, SasState<Canceled>> {
        if let StartEventContent::MSasV1(content) = &event.content {
            let sas = OlmSas::new();
            let utility = OlmUtility::new();

            let json_content = cjson::to_string(&event.content).expect("Can't serialize content");
            let pubkey = sas.public_key();
            let commitment = utility.sha256_utf8_msg(&format!("{}{}", pubkey, json_content));

            let sas = SasState {
                inner: Arc::new(Mutex::new(sas)),

                ids: SasIds {
                    account,
                    other_device,
                },

                verification_flow_id: Arc::new(content.transaction_id.clone()),

                state: Arc::new(Started {
                    protocol_definitions: content.clone(),
                    commitment,
                }),
            };

            if !content
                .key_agreement_protocols
                .contains(&KeyAgreementProtocol::Curve25519HkdfSha256)
                || !content
                    .message_authentication_codes
                    .contains(&MessageAuthenticationCode::HkdfHmacSha256)
                || !content.hashes.contains(&HashAlgorithm::Sha256)
                || (!content
                    .short_authentication_string
                    .contains(&ShortAuthenticationString::Decimal)
                    && !content
                        .short_authentication_string
                        .contains(&ShortAuthenticationString::Emoji))
            {
                Err(sas.cancel(CancelCode::UnknownMethod))
            } else {
                Ok(sas)
            }
        } else {
            Err(SasState {
                inner: Arc::new(Mutex::new(OlmSas::new())),

                ids: SasIds {
                    account,
                    other_device,
                },

                // TODO we can't get to the transaction id currently since it's
                // behind the content specific enum.
                verification_flow_id: Arc::new("".to_owned()),

                state: Arc::new(Canceled::new(CancelCode::UnknownMethod)),
            })
        }
    }

    /// Get the content for the accept event.
    ///
    /// The content needs to be sent to the other device.
    ///
    /// This should be sent out automatically if the SAS verification flow has
    /// been started because of a
    /// m.key.verification.request -> m.key.verification.ready flow.
    fn as_content(&self) -> AcceptEventContent {
        AcceptEventContent {
            method: VerificationMethod::MSasV1,
            transaction_id: self.verification_flow_id.to_string(),
            commitment: self.state.commitment.clone(),
            hash: HashAlgorithm::Sha256,
            key_agreement_protocol: KeyAgreementProtocol::Curve25519HkdfSha256,
            message_authentication_code: MessageAuthenticationCode::HkdfHmacSha256,
            short_authentication_string: self
                .state
                .protocol_definitions
                .short_authentication_string
                .clone(),
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
    fn into_key_received(
        self,
        event: &mut ToDeviceEvent<KeyEventContent>,
    ) -> Result<SasState<KeyReceived>, SasState<Canceled>> {
        self.check_sender_and_txid(&event.sender, &event.content.transaction_id)
            .map_err(|c| self.clone().cancel(c))?;

        let accepted_protocols: AcceptedProtocols = self.as_content().into();
        self.inner
            .lock()
            .unwrap()
            .set_their_public_key(&mem::take(&mut event.content.key))
            .expect("Can't set public key");

        Ok(SasState {
            inner: self.inner,
            ids: self.ids,
            verification_flow_id: self.verification_flow_id,
            state: Arc::new(KeyReceived {
                we_started: false,
                accepted_protocols: Arc::new(accepted_protocols),
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
    fn into_key_received(
        self,
        event: &mut ToDeviceEvent<KeyEventContent>,
    ) -> Result<SasState<KeyReceived>, SasState<Canceled>> {
        self.check_sender_and_txid(&event.sender, &event.content.transaction_id)
            .map_err(|c| self.clone().cancel(c))?;

        let utility = OlmUtility::new();
        let commitment = utility.sha256_utf8_msg(&format!(
            "{}{}",
            event.content.key, self.state.json_start_content
        ));

        if self.state.commitment != commitment {
            Err(self.cancel(CancelCode::InvalidMessage))
        } else {
            self.inner
                .lock()
                .unwrap()
                .set_their_public_key(&mem::take(&mut event.content.key))
                .expect("Can't set public key");

            Ok(SasState {
                inner: self.inner,
                ids: self.ids,
                verification_flow_id: self.verification_flow_id,
                state: Arc::new(KeyReceived {
                    we_started: true,
                    accepted_protocols: self.state.accepted_protocols.clone(),
                }),
            })
        }
    }

    /// Get the content for the key event.
    ///
    /// The content needs to be automatically sent to the other side.
    fn as_content(&self) -> KeyEventContent {
        KeyEventContent {
            transaction_id: self.verification_flow_id.to_string(),
            key: self.inner.lock().unwrap().public_key(),
        }
    }
}

impl SasState<KeyReceived> {
    /// Get the content for the key event.
    ///
    /// The content needs to be automatically sent to the other side if and only
    /// if we_started is false.
    fn as_content(&self) -> KeyEventContent {
        KeyEventContent {
            transaction_id: self.verification_flow_id.to_string(),
            key: self.inner.lock().unwrap().public_key(),
        }
    }

    /// Get the emoji version of the short authentication string.
    ///
    /// Returns a vector of tuples where the first element is the emoji and the
    /// second element the English description of the emoji.
    fn get_emoji(&self) -> Vec<(&'static str, &'static str)> {
        get_emoji(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.verification_flow_id,
            self.state.we_started,
        )
    }

    /// Get the decimal version of the short authentication string.
    ///
    /// Returns a tuple containing three 4 digit integer numbers that represent
    /// the short auth string.
    fn get_decimal(&self) -> (u32, u32, u32) {
        get_decimal(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.verification_flow_id,
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
    fn into_mac_received(
        self,
        event: &ToDeviceEvent<MacEventContent>,
    ) -> Result<SasState<MacReceived>, SasState<Canceled>> {
        self.check_sender_and_txid(&event.sender, &event.content.transaction_id)
            .map_err(|c| self.clone().cancel(c))?;

        let (devices, master_keys) = receive_mac_event(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.verification_flow_id,
            event,
        );

        Ok(SasState {
            inner: self.inner,
            verification_flow_id: self.verification_flow_id,
            ids: self.ids,
            state: Arc::new(MacReceived {
                we_started: self.state.we_started,
                verified_devices: Arc::new(devices),
                verified_master_keys: Arc::new(master_keys),
            }),
        })
    }

    /// Confirm that the short auth string matches.
    ///
    /// This needs to be done by the user, this will put us in the `Confirmed`
    /// state.
    fn confirm(self) -> SasState<Confirmed> {
        SasState {
            inner: self.inner,
            verification_flow_id: self.verification_flow_id,
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
    fn into_done(
        self,
        event: &ToDeviceEvent<MacEventContent>,
    ) -> Result<SasState<Done>, SasState<Canceled>> {
        self.check_sender_and_txid(&event.sender, &event.content.transaction_id)
            .map_err(|c| self.clone().cancel(c))?;
        let (devices, master_keys) = receive_mac_event(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.verification_flow_id,
            event,
        );

        Ok(SasState {
            inner: self.inner,
            verification_flow_id: self.verification_flow_id,
            ids: self.ids,

            state: Arc::new(Done {
                verified_devices: Arc::new(devices),
                verified_master_keys: Arc::new(master_keys),
            }),
        })
    }

    /// Get the content for the mac event.
    ///
    /// The content needs to be automatically sent to the other side.
    fn as_content(&self) -> MacEventContent {
        get_mac_content(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.verification_flow_id,
        )
    }
}

impl SasState<MacReceived> {
    /// Confirm that the short auth string matches.
    ///
    /// This needs to be done by the user, this will put us in the `Done`
    /// state since the other side already confirmed and sent us a MAC event.
    fn confirm(self) -> SasState<Done> {
        SasState {
            inner: self.inner,
            verification_flow_id: self.verification_flow_id,
            ids: self.ids,
            state: Arc::new(Done {
                verified_devices: self.state.verified_devices.clone(),
                verified_master_keys: self.state.verified_master_keys.clone(),
            }),
        }
    }

    /// Get the emoji version of the short authentication string.
    ///
    /// Returns a vector of tuples where the first element is the emoji and the
    /// second element the English description of the emoji.
    fn get_emoji(&self) -> Vec<(&'static str, &'static str)> {
        get_emoji(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.verification_flow_id,
            self.state.we_started,
        )
    }

    /// Get the decimal version of the short authentication string.
    ///
    /// Returns a tuple containing three 4 digit integer numbers that represent
    /// the short auth string.
    fn get_decimal(&self) -> (u32, u32, u32) {
        get_decimal(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.verification_flow_id,
            self.state.we_started,
        )
    }
}

impl SasState<Done> {
    /// Get the content for the mac event.
    ///
    /// The content needs to be automatically sent to the other side if it
    /// wasn't already sent.
    fn as_content(&self) -> MacEventContent {
        get_mac_content(
            &self.inner.lock().unwrap(),
            &self.ids,
            &self.verification_flow_id,
        )
    }

    /// Get the list of verified devices.
    fn verified_devices(&self) -> Arc<Vec<Box<DeviceId>>> {
        self.state.verified_devices.clone()
    }

    /// Get the list of verified master keys.
    fn verified_master_keys(&self) -> Arc<Vec<String>> {
        self.state.verified_master_keys.clone()
    }
}

impl Canceled {
    fn new(code: CancelCode) -> Canceled {
        let reason = match code {
            CancelCode::Accepted => {
                "A m.key.verification.request was accepted by a different device."
            }
            CancelCode::InvalidMessage => "The received message was invalid.",
            CancelCode::KeyMismatch => "The expected key did not match the verified one",
            CancelCode::Timeout => "The verification process timed out.",
            CancelCode::UnexpectedMessage => "The device received an unexpected message.",
            CancelCode::UnknownMethod => {
                "The device does not know how to handle the requested method."
            }
            CancelCode::UnknownTransaction => {
                "The device does not know about the given transaction ID."
            }
            CancelCode::User => "The user cancelled the verification.",
            CancelCode::UserMismatch => "The expected user did not match the verified user",
            _ => unimplemented!(),
        };

        Canceled {
            cancel_code: code,
            reason,
        }
    }
}

impl SasState<Canceled> {
    fn as_content(&self) -> AnyToDeviceEventContent {
        AnyToDeviceEventContent::KeyVerificationCancel(CancelEventContent {
            transaction_id: self.verification_flow_id.to_string(),
            reason: self.state.reason.to_string(),
            code: self.state.cancel_code.clone(),
        })
    }
}

#[cfg(test)]
mod test {
    use std::convert::TryFrom;

    use crate::verification::test::wrap_any_to_device_content;
    use crate::{Account, Device};
    use matrix_sdk_common::events::{AnyToDeviceEvent, EventContent, ToDeviceEvent};
    use matrix_sdk_common::identifiers::{DeviceId, UserId};

    use super::{Accepted, Created, Sas, SasState, Started};

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

    fn wrap_to_device_event<C: EventContent>(sender: &UserId, content: C) -> ToDeviceEvent<C> {
        ToDeviceEvent {
            sender: sender.clone(),
            content,
        }
    }

    async fn get_sas_pair() -> (SasState<Created>, SasState<Started>) {
        let alice = Account::new(&alice_id(), &alice_device_id());
        let alice_device = Device::from_account(&alice).await;

        let bob = Account::new(&bob_id(), &bob_device_id());
        let bob_device = Device::from_account(&bob).await;

        let alice_sas = SasState::<Created>::new(alice.clone(), bob_device);

        let start_content = alice_sas.as_content();
        let event = wrap_to_device_event(alice_sas.user_id(), start_content);

        let bob_sas = SasState::<Started>::from_start_event(bob.clone(), alice_device, &event);

        (alice_sas, bob_sas.unwrap())
    }

    #[tokio::test]
    async fn create_sas() {
        let (_, _) = get_sas_pair().await;
    }

    #[tokio::test]
    async fn sas_accept() {
        let (alice, bob) = get_sas_pair().await;

        let event = wrap_to_device_event(bob.user_id(), bob.as_content());

        alice.into_accepted(&event).unwrap();
    }

    #[tokio::test]
    async fn sas_key_share() {
        let (alice, bob) = get_sas_pair().await;

        let event = wrap_to_device_event(bob.user_id(), bob.as_content());

        let alice: SasState<Accepted> = alice.into_accepted(&event).unwrap();
        let mut event = wrap_to_device_event(alice.user_id(), alice.as_content());

        let bob = bob.into_key_received(&mut event).unwrap();

        let mut event = wrap_to_device_event(bob.user_id(), bob.as_content());

        let alice = alice.into_key_received(&mut event).unwrap();

        assert_eq!(alice.get_decimal(), bob.get_decimal());
        assert_eq!(alice.get_emoji(), bob.get_emoji());
    }

    #[tokio::test]
    async fn sas_full() {
        let (alice, bob) = get_sas_pair().await;

        let event = wrap_to_device_event(bob.user_id(), bob.as_content());

        let alice: SasState<Accepted> = alice.into_accepted(&event).unwrap();
        let mut event = wrap_to_device_event(alice.user_id(), alice.as_content());

        let bob = bob.into_key_received(&mut event).unwrap();

        let mut event = wrap_to_device_event(bob.user_id(), bob.as_content());

        let alice = alice.into_key_received(&mut event).unwrap();

        assert_eq!(alice.get_decimal(), bob.get_decimal());
        assert_eq!(alice.get_emoji(), bob.get_emoji());

        let bob = bob.confirm();

        let event = wrap_to_device_event(bob.user_id(), bob.as_content());

        let alice = alice.into_mac_received(&event).unwrap();
        assert!(!alice.get_emoji().is_empty());
        let alice = alice.confirm();

        let event = wrap_to_device_event(alice.user_id(), alice.as_content());
        let bob = bob.into_done(&event).unwrap();

        assert!(bob.verified_devices().contains(&alice.device_id().into()));
        assert!(alice.verified_devices().contains(&bob.device_id().into()));
    }

    #[tokio::test]
    async fn sas_wrapper_full() {
        let alice = Account::new(&alice_id(), &alice_device_id());
        let alice_device = Device::from_account(&alice).await;

        let bob = Account::new(&bob_id(), &bob_device_id());
        let bob_device = Device::from_account(&bob).await;

        let (alice, content) = Sas::start(alice, bob_device);
        let event = wrap_to_device_event(alice.user_id(), content);

        let bob = Sas::from_start_event(bob, alice_device, &event).unwrap();
        let event = wrap_to_device_event(bob.user_id(), bob.accept().unwrap());

        let content = alice.receive_event(&mut AnyToDeviceEvent::KeyVerificationAccept(event));

        assert!(!alice.can_be_presented());
        assert!(!bob.can_be_presented());

        let mut event = wrap_any_to_device_content(alice.user_id(), content.unwrap());
        let mut event =
            wrap_any_to_device_content(bob.user_id(), bob.receive_event(&mut event).unwrap());

        assert!(bob.can_be_presented());

        alice.receive_event(&mut event);
        assert!(alice.can_be_presented());

        assert_eq!(alice.emoji().unwrap(), bob.emoji().unwrap());
        assert_eq!(alice.decimals().unwrap(), bob.decimals().unwrap());

        let event = wrap_to_device_event(alice.user_id(), alice.confirm().unwrap());
        bob.receive_event(&mut AnyToDeviceEvent::KeyVerificationMac(event));

        let event = wrap_to_device_event(bob.user_id(), bob.confirm().unwrap());
        alice.receive_event(&mut AnyToDeviceEvent::KeyVerificationMac(event));

        assert!(alice
            .verified_devices()
            .unwrap()
            .contains(&bob.device_id().into()));
        assert!(bob
            .verified_devices()
            .unwrap()
            .contains(&alice.device_id().into()));
    }
}
