use crate::Device;

use olm_rs::sas::OlmSas;

use matrix_sdk_common::events::{
    key::verification::{
        accept::AcceptEvent,
        key::KeyEvent,
        mac::MacEvent,
        start::{MSasV1Content, MSasV1ContentOptions, StartEvent, StartEventContent},
        HashAlgorithm, KeyAgreementProtocol, MessageAuthenticationCode, ShortAuthenticationString,
        VerificationMethod,
    },
    ToDeviceEvent,
};
use matrix_sdk_common::identifiers::{DeviceId, UserId};
use matrix_sdk_common::uuid::Uuid;

struct SasIds {
    own_user_id: UserId,
    own_device_id: Box<DeviceId>,
    other_device: Device,
}

struct AcceptedProtocols {
    method: VerificationMethod,
    key_agreement_protocol: KeyAgreementProtocol,
    hash: HashAlgorithm,
    message_auth_code: MessageAuthenticationCode,
    short_auth_string: Vec<ShortAuthenticationString>,
}

struct Sas<S> {
    inner: OlmSas,
    ids: SasIds,
    verification_flow_id: Uuid,
    state: S,
}

impl<S> Sas<S> {
    pub fn user_id(&self) -> &UserId {
        &self.ids.own_user_id
    }
}

impl Sas<Created> {
    fn new(own_user_id: UserId, own_device_id: &DeviceId, other_device: Device) -> Sas<Created> {
        let verification_flow_id = Uuid::new_v4();

        Sas {
            inner: OlmSas::new(),
            ids: SasIds {
                own_user_id,
                own_device_id: own_device_id.into(),
                other_device,
            },
            verification_flow_id,

            state: Created {
                protocol_definitions: MSasV1ContentOptions {
                    transaction_id: verification_flow_id.to_string(),
                    from_device: own_device_id.into(),
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

    fn into_accepted(self, event: &AcceptEvent) -> Sas<Accepted> {
        let content = &event.content;

        Sas {
            inner: self.inner,
            ids: self.ids,
            verification_flow_id: self.verification_flow_id,
            state: Accepted {
                commitment: content.commitment.clone(),
                accepted_protocols: AcceptedProtocols {
                    method: content.method,
                    hash: content.hash,
                    key_agreement_protocol: content.key_agreement_protocol,
                    message_auth_code: content.message_authentication_code,
                    short_auth_string: content.short_authentication_string.clone(),
                },
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
        own_user_id: &UserId,
        own_device_id: &DeviceId,
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
                own_user_id: own_user_id.clone(),
                own_device_id: own_device_id.into(),
                other_device,
            },

            verification_flow_id: Uuid::new_v4(),

            state: Started {
                protocol_definitions: content.clone(),
            },
        }
    }

    fn into_key_received(self, event: &KeyEvent) -> Sas<KeyReceived> {
        todo!()
    }
}

struct Accepted {
    accepted_protocols: AcceptedProtocols,
    commitment: String,
}

impl Sas<Accepted> {
    fn into_key_received(self, event: &KeyEvent) -> Sas<KeyReceived> {
        todo!()
    }
}

struct KeyReceived {
    accepted_protocols: AcceptedProtocols,
}

impl Sas<KeyReceived> {
    fn into_mac_received(self, event: &MacEvent) -> Sas<MacReceived> {
        todo!()
    }

    fn confirm(self) -> Sas<Confirmed> {
        todo!()
    }
}

struct Confirmed {
    accepted_protocols: AcceptedProtocols,
}

impl Sas<Confirmed> {
    fn confirm(self) -> Sas<Done> {
        todo!()
    }
}

struct MacReceived {
    verified_devices: Vec<String>,
    verified_master_keys: Vec<String>,
}

impl Sas<MacReceived> {
    fn into_done(self, event: &MacEvent) -> Sas<Done> {
        todo!()
    }
}

struct Done {
    verified_devices: Vec<String>,
    verified_master_keys: Vec<String>,
}

#[cfg(test)]
mod test {
    use std::convert::TryFrom;

    use crate::{Account, Device};
    use matrix_sdk_common::events::key::verification::{
        accept::AcceptEvent,
        key::KeyEvent,
        mac::MacEvent,
        start::{MSasV1Content, MSasV1ContentOptions, StartEvent, StartEventContent},
    };
    use matrix_sdk_common::events::{AnyToDeviceEvent, ToDeviceEvent};
    use matrix_sdk_common::identifiers::{DeviceId, UserId};

    use super::{Created, Sas, Started};

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

    fn wrap_start_event(
        sender: &UserId,
        content: StartEventContent,
    ) -> ToDeviceEvent<StartEventContent> {
        ToDeviceEvent {
            sender: sender.clone(),
            content,
        }
    }

    #[tokio::test]
    async fn create_sas() {
        let alice = Account::new(&alice_id(), &alice_device_id());
        let alice_device = Device::from_account(&alice).await;

        let bob = Account::new(&bob_id(), &bob_device_id());
        let bob_device = Device::from_account(&bob).await;

        let alice_sas = Sas::<Created>::new(alice_id(), &alice_device_id(), bob_device);

        let start_content = alice_sas.get_start_event();
        let event = wrap_start_event(alice_sas.user_id(), start_content);

        let bob_sas =
            Sas::<Started>::from_start_event(bob.user_id(), bob.device_id(), alice_device, &event);
    }
}
