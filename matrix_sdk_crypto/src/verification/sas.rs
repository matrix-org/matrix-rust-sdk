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

use super::{get_emoji, get_mac_content, receive_mac_event, SasIds};
use crate::{Account, Device};

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
            short_auth_string: content.short_authentication_string.clone(),
        }
    }
}

struct Sas<S> {
    inner: OlmSas,
    ids: SasIds,
    verification_flow_id: String,
    state: S,
}

impl<S> Sas<S> {
    pub fn user_id(&self) -> &UserId {
        &self.ids.account.user_id()
    }

    pub fn device_id(&self) -> &DeviceId {
        &self.ids.account.device_id()
    }
}

impl Sas<Created> {
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

    fn get_start_event(&self) -> StartEventContent {
        StartEventContent::MSasV1(
            MSasV1Content::new(self.state.protocol_definitions.clone())
                .expect("Invalid initial protocol definitions."),
        )
    }

    fn into_accepted(self, event: &ToDeviceEvent<AcceptEventContent>) -> Sas<Accepted> {
        let content = &event.content;

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

struct Created {
    protocol_definitions: MSasV1ContentOptions,
}

struct Started {
    protocol_definitions: MSasV1Content,
}

impl Sas<Started> {
    fn from_start_event(
        account: Account,
        other_device: Device,
        event: &ToDeviceEvent<StartEventContent>,
    ) -> Sas<Started> {
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

    fn get_accept_content(&self) -> AcceptEventContent {
        AcceptEventContent {
            method: VerificationMethod::MSasV1,
            transaction_id: self.verification_flow_id.to_string(),
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

struct Accepted {
    accepted_protocols: AcceptedProtocols,
    commitment: String,
}

impl Sas<Accepted> {
    fn into_key_received(mut self, event: &mut ToDeviceEvent<KeyEventContent>) -> Sas<KeyReceived> {
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

    fn get_key_content(&self) -> KeyEventContent {
        KeyEventContent {
            transaction_id: self.verification_flow_id.to_string(),
            key: self.inner.public_key(),
        }
    }
}

struct KeyReceived {
    we_started: bool,
    accepted_protocols: AcceptedProtocols,
}

impl Sas<KeyReceived> {
    fn get_key_content(&self) -> KeyEventContent {
        KeyEventContent {
            transaction_id: self.verification_flow_id.to_string(),
            key: self.inner.public_key(),
        }
    }

    fn extra_info(&self) -> String {
        if self.state.we_started {
            format!(
                "MATRIX_KEY_VERIFICATION_SAS{first_user}{first_device}\
                {second_user}{second_device}{transaction_id}",
                first_user = self.ids.account.user_id(),
                first_device = self.ids.account.device_id(),
                second_user = self.ids.other_device.user_id(),
                second_device = self.ids.other_device.device_id(),
                transaction_id = self.verification_flow_id,
            )
        } else {
            format!(
                "MATRIX_KEY_VERIFICATION_SAS{first_user}{first_device}\
                {second_user}{second_device}{transaction_id}",
                first_user = self.ids.other_device.user_id(),
                first_device = self.ids.other_device.device_id(),
                second_user = self.ids.account.user_id(),
                second_device = self.ids.account.device_id(),
                transaction_id = self.verification_flow_id,
            )
        }
    }

    fn get_emoji(&self) -> Vec<(&'static str, &'static str)> {
        let bytes: Vec<u64> = self
            .inner
            .generate_bytes(&self.extra_info(), 6)
            .expect("Can't generate bytes")
            .into_iter()
            .map(|b| b as u64)
            .collect();

        let mut num: u64 = bytes[0] << 40;
        num += bytes[1] << 32;
        num += bytes[2] << 24;
        num += bytes[3] << 16;
        num += bytes[4] << 8;
        num += bytes[5];

        let numbers = vec![
            ((num >> 42) & 63) as u8,
            ((num >> 36) & 63) as u8,
            ((num >> 30) & 63) as u8,
            ((num >> 24) & 63) as u8,
            ((num >> 18) & 63) as u8,
            ((num >> 12) & 63) as u8,
            ((num >> 6) & 63) as u8,
        ];

        numbers.into_iter().map(get_emoji).collect()
    }

    fn get_decimal(&self) -> (u32, u32, u32) {
        let bytes: Vec<u32> = self
            .inner
            .generate_bytes(&self.extra_info(), 5)
            .expect("Can't generate bytes")
            .into_iter()
            .map(|b| b as u32)
            .collect();

        let first = bytes[0] << 5 | bytes[1] >> 3;
        let second = (bytes[1] & 0x7) << 10 | bytes[2] << 2 | bytes[3] >> 6;
        let third = (bytes[3] & 0x3F) << 7 | bytes[4] >> 1;

        (first + 1000, second + 1000, third + 1000)
    }

    fn into_mac_received(self, event: &ToDeviceEvent<MacEventContent>) -> Sas<MacReceived> {
        let (devices, master_keys) =
            receive_mac_event(&self.inner, &self.ids, &self.verification_flow_id, event);
        Sas {
            inner: self.inner,
            verification_flow_id: self.verification_flow_id,
            ids: self.ids,
            state: MacReceived {
                verified_devices: devices,
                verified_master_keys: master_keys,
            },
        }
    }

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

struct Confirmed {
    accepted_protocols: AcceptedProtocols,
}

impl Sas<Confirmed> {
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

    fn get_mac_event_content(&self) -> MacEventContent {
        get_mac_content(&self.inner, &self.ids, &self.verification_flow_id)
    }
}

struct MacReceived {
    verified_devices: Vec<Box<DeviceId>>,
    verified_master_keys: Vec<String>,
}

impl Sas<MacReceived> {
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
}

struct Done {
    verified_devices: Vec<Box<DeviceId>>,
    verified_master_keys: Vec<String>,
}

impl Sas<Done> {
    fn get_mac_event_content(&self) -> MacEventContent {
        get_mac_content(&self.inner, &self.ids, &self.verification_flow_id)
    }

    fn verified_devices(&self) -> &Vec<Box<DeviceId>> {
        &self.state.verified_devices
    }

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
    async fn sas_mac() {
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
        // assert!(!alice.get_emoji().is_empty());
        let alice = alice.confirm();

        let event = wrap_to_device_event(alice.user_id(), alice.get_mac_event_content());
        let bob = bob.into_done(&event);

        assert!(bob.verified_devices().contains(&alice.device_id().into()));
        assert!(alice.verified_devices().contains(&bob.device_id().into()));
    }
}
