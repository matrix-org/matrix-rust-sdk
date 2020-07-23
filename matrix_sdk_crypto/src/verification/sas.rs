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

use std::mem;

use olm_rs::sas::OlmSas;

use matrix_sdk_common::events::{
    key::verification::{
        accept::AcceptEventContent,
        key::KeyEventContent,
        mac::MacEventContent,
        start::{MSasV1Content, MSasV1ContentOptions, StartEventContent},
        HashAlgorithm, KeyAgreementProtocol, MessageAuthenticationCode, ShortAuthenticationString,
        VerificationMethod,
    },
    ToDeviceEvent,
};
use matrix_sdk_common::identifiers::{DeviceId, UserId};
use matrix_sdk_common::uuid::Uuid;

use super::{get_decimal, get_emoji, get_mac_content, receive_mac_event, SasIds};
use crate::{Account, Device};

/// Struct containing the protocols that were agreed to be used for the SAS
/// flow.
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

// TODO each of our state transitions can fail and return a canceled state. We
// need to check the senders at each transition, the commitment, the
// verification flow id (transaction id).

/// A type level state machine modeling the Sas flow.
///
/// This is the generic struc holding common data between the different states
/// and the specific state.
struct Sas<S> {
    /// The Olm SAS struct.
    inner: OlmSas,
    /// Struct holding the identities that are doing the SAS dance.
    ids: SasIds,
    /// The unique identifier of this SAS flow.
    ///
    /// This will be the transaction id for to-device events and the relates_to
    /// field for in-room events.
    verification_flow_id: String,
    /// The SAS state we're in.
    state: S,
}

/// The initial SAS state.
struct Created {
    protocol_definitions: MSasV1ContentOptions,
}

/// The initial SAS state if the other side started the SAS verification.
struct Started {
    protocol_definitions: MSasV1Content,
}

/// The SAS state we're going to be in after the other side accepted our
/// verification start event.
struct Accepted {
    accepted_protocols: AcceptedProtocols,
    commitment: String,
}

/// The SAS state we're going to be in after we received the public key of the
/// other participant.
///
/// From now on we can show the short auth string to the user.
struct KeyReceived {
    we_started: bool,
    accepted_protocols: AcceptedProtocols,
}

/// The SAS state we're going to be in after the user has confirmed that the
/// short auth string matches. We still need to receive a MAC event from the
/// other side.
struct Confirmed {
    accepted_protocols: AcceptedProtocols,
}

/// The SAS state we're going to be in after we receive a MAC event from the
/// other side. Our own user still needs to confirm that the short auth string
/// matches.
struct MacReceived {
    we_started: bool,
    verified_devices: Vec<Box<DeviceId>>,
    verified_master_keys: Vec<String>,
}

/// The SAS state indicating that the verification finished successfully.
///
/// We can now mark the device in our verified devices lits as verified and sign
/// the master keys in the verified devices list.
struct Done {
    verified_devices: Vec<Box<DeviceId>>,
    verified_master_keys: Vec<String>,
}

impl<S> Sas<S> {
    /// Get our own user id.
    pub fn user_id(&self) -> &UserId {
        &self.ids.account.user_id()
    }

    /// Get our own device id.
    pub fn device_id(&self) -> &DeviceId {
        &self.ids.account.device_id()
    }
}

impl Sas<Created> {
    /// Create a new SAS verification flow.
    ///
    /// # Arguments
    ///
    /// * `account` - Our own account.
    ///
    /// * `other_device` - The other device which we are going to verify.
    fn new(account: Account, other_device: Device) -> Sas<Created> {
        let verification_flow_id = Uuid::new_v4().to_string();
        let from_device: Box<DeviceId> = account.device_id().into();

        Sas {
            inner: OlmSas::new(),
            ids: SasIds {
                account,
                other_device,
            },
            verification_flow_id: verification_flow_id.clone(),

            state: Created {
                protocol_definitions: MSasV1ContentOptions {
                    transaction_id: verification_flow_id,
                    from_device,
                    short_authentication_string: vec![
                        ShortAuthenticationString::Decimal,
                        ShortAuthenticationString::Emoji,
                    ],
                    key_agreement_protocols: vec![KeyAgreementProtocol::Curve25519HkdfSha256],
                    message_authentication_codes: vec![MessageAuthenticationCode::HkdfHmacSha256],
                    hashes: vec![HashAlgorithm::Sha256],
                },
            },
        }
    }

    /// Get the content for the start event.
    ///
    /// The content needs to be sent to the other device.
    fn get_start_event(&self) -> StartEventContent {
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
    fn into_accepted(self, event: &ToDeviceEvent<AcceptEventContent>) -> Sas<Accepted> {
        let content = &event.content;

        // TODO check that we support the agreed upon protocols, cancel if not.

        Sas {
            inner: self.inner,
            ids: self.ids,
            verification_flow_id: self.verification_flow_id,
            state: Accepted {
                commitment: content.commitment.clone(),
                accepted_protocols: content.clone().into(),
            },
        }
    }
}

impl Sas<Started> {
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
    ) -> Sas<Started> {
        // TODO check if we support the suggested protocols and cancel if we
        // don't
        let content = if let StartEventContent::MSasV1(content) = &event.content {
            content
        } else {
            panic!("Invalid sas version")
        };

        Sas {
            inner: OlmSas::new(),

            ids: SasIds {
                account,
                other_device,
            },

            verification_flow_id: content.transaction_id.clone(),

            state: Started {
                protocol_definitions: content.clone(),
            },
        }
    }

    /// Get the content for the accept event.
    ///
    /// The content needs to be sent to the other device.
    ///
    /// This should be sent out automatically if the SAS verification flow has
    /// been started because of a
    /// m.key.verification.request -> m.key.verification.ready flow.
    fn get_accept_content(&self) -> AcceptEventContent {
        AcceptEventContent {
            method: VerificationMethod::MSasV1,
            transaction_id: self.verification_flow_id.to_string(),
            // TODO calculate the commitment.
            commitment: "".to_owned(),
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
    fn into_key_received(mut self, event: &mut ToDeviceEvent<KeyEventContent>) -> Sas<KeyReceived> {
        let accepted_protocols: AcceptedProtocols = self.get_accept_content().into();
        self.inner
            .set_their_public_key(&mem::take(&mut event.content.key))
            .expect("Can't set public key");

        Sas {
            inner: self.inner,
            ids: self.ids,
            verification_flow_id: self.verification_flow_id,
            state: KeyReceived {
                we_started: false,
                accepted_protocols,
            },
        }
    }
}

impl Sas<Accepted> {
    /// Receive a m.key.verification.key event, changing the state into
    /// a `KeyReceived` one
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.key event that was sent to us by
    /// the other side. The event will be modified so it doesn't contain any key
    /// anymore.
    fn into_key_received(mut self, event: &mut ToDeviceEvent<KeyEventContent>) -> Sas<KeyReceived> {
        // TODO check the commitment here since we started the SAS dance.
        self.inner
            .set_their_public_key(&mem::take(&mut event.content.key))
            .expect("Can't set public key");

        Sas {
            inner: self.inner,
            ids: self.ids,
            verification_flow_id: self.verification_flow_id,
            state: KeyReceived {
                we_started: true,
                accepted_protocols: self.state.accepted_protocols,
            },
        }
    }

    /// Get the content for the key event.
    ///
    /// The content needs to be automatically sent to the other side.
    fn get_key_content(&self) -> KeyEventContent {
        KeyEventContent {
            transaction_id: self.verification_flow_id.to_string(),
            key: self.inner.public_key(),
        }
    }
}

impl Sas<KeyReceived> {
    /// Get the content for the key event.
    ///
    /// The content needs to be automatically sent to the other side if and only
    /// if we_started is false.
    fn get_key_content(&self) -> KeyEventContent {
        KeyEventContent {
            transaction_id: self.verification_flow_id.to_string(),
            key: self.inner.public_key(),
        }
    }

    /// Get the emoji version of the short authentication string.
    ///
    /// Returns a vector of tuples where the first element is the emoji and the
    /// second element the English description of the emoji.
    fn get_emoji(&self) -> Vec<(&'static str, &'static str)> {
        get_emoji(
            &self.inner,
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
            &self.inner,
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
    fn into_mac_received(self, event: &ToDeviceEvent<MacEventContent>) -> Sas<MacReceived> {
        let (devices, master_keys) =
            receive_mac_event(&self.inner, &self.ids, &self.verification_flow_id, event);
        Sas {
            inner: self.inner,
            verification_flow_id: self.verification_flow_id,
            ids: self.ids,
            state: MacReceived {
                we_started: self.state.we_started,
                verified_devices: devices,
                verified_master_keys: master_keys,
            },
        }
    }

    /// Confirm that the short auth string matches.
    ///
    /// This needs to be done by the user, this will put us in the `Confirmed`
    /// state.
    fn confirm(self) -> Sas<Confirmed> {
        Sas {
            inner: self.inner,
            verification_flow_id: self.verification_flow_id,
            ids: self.ids,
            state: Confirmed {
                accepted_protocols: self.state.accepted_protocols,
            },
        }
    }
}

impl Sas<Confirmed> {
    /// Receive a m.key.verification.mac event, changing the state into
    /// a `Done` one
    ///
    /// # Arguments
    ///
    /// * `event` - The m.key.verification.mac event that was sent to us by
    /// the other side.
    fn into_done(self, event: &ToDeviceEvent<MacEventContent>) -> Sas<Done> {
        let (devices, master_keys) =
            receive_mac_event(&self.inner, &self.ids, &self.verification_flow_id, event);

        Sas {
            inner: self.inner,
            verification_flow_id: self.verification_flow_id,
            ids: self.ids,

            state: Done {
                verified_devices: devices,
                verified_master_keys: master_keys,
            },
        }
    }

    /// Get the content for the mac event.
    ///
    /// The content needs to be automatically sent to the other side.
    fn get_mac_event_content(&self) -> MacEventContent {
        get_mac_content(&self.inner, &self.ids, &self.verification_flow_id)
    }
}

impl Sas<MacReceived> {
    /// Confirm that the short auth string matches.
    ///
    /// This needs to be done by the user, this will put us in the `Done`
    /// state since the other side already confirmed and sent us a MAC event.
    fn confirm(self) -> Sas<Done> {
        Sas {
            inner: self.inner,
            verification_flow_id: self.verification_flow_id,
            ids: self.ids,
            state: Done {
                verified_devices: self.state.verified_devices,
                verified_master_keys: self.state.verified_master_keys,
            },
        }
    }

    /// Get the emoji version of the short authentication string.
    ///
    /// Returns a vector of tuples where the first element is the emoji and the
    /// second element the English description of the emoji.
    fn get_emoji(&self) -> Vec<(&'static str, &'static str)> {
        get_emoji(
            &self.inner,
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
            &self.inner,
            &self.ids,
            &self.verification_flow_id,
            self.state.we_started,
        )
    }
}

impl Sas<Done> {
    /// Get the content for the mac event.
    ///
    /// The content needs to be automatically sent to the other side if it
    /// wasn't already sent.
    fn get_mac_event_content(&self) -> MacEventContent {
        get_mac_content(&self.inner, &self.ids, &self.verification_flow_id)
    }

    /// Get the list of verified devices.
    fn verified_devices(&self) -> &Vec<Box<DeviceId>> {
        &self.state.verified_devices
    }

    /// Get the list of verified master keys.
    fn verified_master_keys(&self) -> &[String] {
        &self.state.verified_master_keys
    }
}

#[cfg(test)]
mod test {
    use std::convert::TryFrom;

    use crate::{Account, Device};
    use matrix_sdk_common::events::{EventContent, ToDeviceEvent};
    use matrix_sdk_common::identifiers::{DeviceId, UserId};

    use super::{Accepted, Created, Sas, Started};

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

    async fn get_sas_pair() -> (Sas<Created>, Sas<Started>) {
        let alice = Account::new(&alice_id(), &alice_device_id());
        let alice_device = Device::from_account(&alice).await;

        let bob = Account::new(&bob_id(), &bob_device_id());
        let bob_device = Device::from_account(&bob).await;

        let alice_sas = Sas::<Created>::new(alice.clone(), bob_device);

        let start_content = alice_sas.get_start_event();
        let event = wrap_to_device_event(alice_sas.user_id(), start_content);

        let bob_sas = Sas::<Started>::from_start_event(bob.clone(), alice_device, &event);

        (alice_sas, bob_sas)
    }

    #[tokio::test]
    async fn create_sas() {
        let (_, _) = get_sas_pair().await;
    }

    #[tokio::test]
    async fn sas_accept() {
        let (alice, bob) = get_sas_pair().await;

        let event = wrap_to_device_event(bob.user_id(), bob.get_accept_content());

        alice.into_accepted(&event);
    }

    #[tokio::test]
    async fn sas_key_share() {
        let (alice, bob) = get_sas_pair().await;

        let event = wrap_to_device_event(bob.user_id(), bob.get_accept_content());

        let alice: Sas<Accepted> = alice.into_accepted(&event);
        let mut event = wrap_to_device_event(alice.user_id(), alice.get_key_content());

        let bob = bob.into_key_received(&mut event);

        let mut event = wrap_to_device_event(bob.user_id(), bob.get_key_content());

        let alice = alice.into_key_received(&mut event);

        assert_eq!(alice.get_decimal(), bob.get_decimal());
        assert_eq!(alice.get_emoji(), bob.get_emoji());
    }

    #[tokio::test]
    async fn sas_full() {
        let (alice, bob) = get_sas_pair().await;

        let event = wrap_to_device_event(bob.user_id(), bob.get_accept_content());

        let alice: Sas<Accepted> = alice.into_accepted(&event);
        let mut event = wrap_to_device_event(alice.user_id(), alice.get_key_content());

        let bob = bob.into_key_received(&mut event);

        let mut event = wrap_to_device_event(bob.user_id(), bob.get_key_content());

        let alice = alice.into_key_received(&mut event);

        assert_eq!(alice.get_decimal(), bob.get_decimal());
        assert_eq!(alice.get_emoji(), bob.get_emoji());

        let bob = bob.confirm();

        let event = wrap_to_device_event(bob.user_id(), bob.get_mac_event_content());

        let alice = alice.into_mac_received(&event);
        assert!(!alice.get_emoji().is_empty());
        let alice = alice.confirm();

        let event = wrap_to_device_event(alice.user_id(), alice.get_mac_event_content());
        let bob = bob.into_done(&event);

        assert!(bob.verified_devices().contains(&alice.device_id().into()));
        assert!(alice.verified_devices().contains(&bob.device_id().into()));
    }
}
