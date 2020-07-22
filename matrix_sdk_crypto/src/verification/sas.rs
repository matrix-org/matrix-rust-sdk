use crate::Device;

use matrix_sdk_common::events::key::verification::{
    accept::AcceptEvent,
    start::{StartEvent, StartEventContent},
    HashAlgorithm, KeyAgreementProtocol, MessageAuthenticationCode, ShortAuthenticationString,
    VerificationMethod,
};
use matrix_sdk_common::identifiers::{DeviceId, UserId};
use matrix_sdk_common::uuid::Uuid;

struct SasIds {
    own_user_id: UserId,
    own_device_id: Box<DeviceId>,
    other_device: Device,
}

struct ProtocolDefinitions {
    key_agreement_protocols: Vec<KeyAgreementProtocol>,
    hashes: Vec<HashAlgorithm>,
    message_auth_codes: Vec<MessageAuthenticationCode>,
    short_auth_string: Vec<ShortAuthenticationString>,
}

struct AcceptedProtocols {
    method: VerificationMethod,
    key_agreement_protocol: KeyAgreementProtocol,
    hash: HashAlgorithm,
    message_auth_code: MessageAuthenticationCode,
    short_auth_string: Vec<ShortAuthenticationString>,
}

struct Sas<S> {
    ids: SasIds,
    verification_flow_id: Uuid,
    protocol_definitions: ProtocolDefinitions,
    state: S,
}

impl Sas<Created> {
    fn new(own_user_id: UserId, own_device_id: &DeviceId, other_device: Device) -> Sas<Created> {
        Sas {
            ids: SasIds {
                own_user_id,
                own_device_id: own_device_id.into(),
                other_device,
            },
            verification_flow_id: Uuid::new_v4(),

            protocol_definitions: ProtocolDefinitions {
                short_auth_string: vec![
                    ShortAuthenticationString::Decimal,
                    ShortAuthenticationString::Emoji,
                ],
                key_agreement_protocols: vec![KeyAgreementProtocol::Curve25519],
                message_auth_codes: vec![MessageAuthenticationCode::HkdfHmacSha256],
                hashes: vec![HashAlgorithm::Sha256],
            },

            state: Created {},
        }
    }

    fn into_accepted(self, event: &AcceptEvent) -> Sas<Accepted> {
        let content = &event.content;

        Sas {
            ids: self.ids,
            verification_flow_id: self.verification_flow_id,
            protocol_definitions: self.protocol_definitions,
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

struct Created {}

struct Started {}

impl Sas<Started> {
    fn from_start_event(
        own_user_id: UserId,
        own_device_id: &DeviceId,
        other_device: Device,
        event: &StartEvent,
    ) -> Sas<Started> {
        let content = if let StartEventContent::MSasV1(content) = &event.content {
            content
        } else {
            panic!("Invalid sas version")
        };

        Sas {
            ids: SasIds {
                own_user_id,
                own_device_id: own_device_id.into(),
                other_device,
            },
            verification_flow_id: Uuid::new_v4(),

            protocol_definitions: ProtocolDefinitions {
                short_auth_string: content.short_authentication_string.clone(),
                key_agreement_protocols: content.key_agreement_protocols.clone(),
                message_auth_codes: content.message_authentication_codes.clone(),
                hashes: content.hashes.clone(),
            },

            state: Started {},
        }
    }
}

struct Accepted {
    accepted_protocols: AcceptedProtocols,
    commitment: String,
}
