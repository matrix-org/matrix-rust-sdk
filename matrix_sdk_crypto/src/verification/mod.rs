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

mod machine;
mod requests;
mod sas;

pub use machine::VerificationMachine;
pub use requests::VerificationRequest;
pub use sas::{AcceptSettings, Sas, VerificationResult};

#[cfg(test)]
pub(crate) mod test {
    use matrix_sdk_common::{
        events::{AnyToDeviceEvent, AnyToDeviceEventContent, EventType, ToDeviceEvent},
        identifiers::UserId,
    };
    use serde_json::Value;

    use super::sas::OutgoingContent;
    use crate::{
        requests::{OutgoingRequest, OutgoingRequests},
        OutgoingVerificationRequest,
    };

    pub(crate) fn request_to_event(
        sender: &UserId,
        request: &OutgoingVerificationRequest,
    ) -> AnyToDeviceEvent {
        let content = get_content_from_request(request);
        wrap_any_to_device_content(sender, content)
    }

    pub(crate) fn outgoing_request_to_event(
        sender: &UserId,
        request: &OutgoingRequest,
    ) -> AnyToDeviceEvent {
        match request.request() {
            OutgoingRequests::ToDeviceRequest(r) => request_to_event(sender, &r.clone().into()),
            _ => panic!("Unsupported outgoing request"),
        }
    }

    pub(crate) fn wrap_any_to_device_content(
        sender: &UserId,
        content: OutgoingContent,
    ) -> AnyToDeviceEvent {
        let content = if let OutgoingContent::ToDevice(c) = content { c } else { unreachable!() };

        match content {
            AnyToDeviceEventContent::KeyVerificationKey(c) => {
                AnyToDeviceEvent::KeyVerificationKey(ToDeviceEvent {
                    sender: sender.clone(),
                    content: c,
                })
            }
            AnyToDeviceEventContent::KeyVerificationStart(c) => {
                AnyToDeviceEvent::KeyVerificationStart(ToDeviceEvent {
                    sender: sender.clone(),
                    content: c,
                })
            }
            AnyToDeviceEventContent::KeyVerificationAccept(c) => {
                AnyToDeviceEvent::KeyVerificationAccept(ToDeviceEvent {
                    sender: sender.clone(),
                    content: c,
                })
            }
            AnyToDeviceEventContent::KeyVerificationMac(c) => {
                AnyToDeviceEvent::KeyVerificationMac(ToDeviceEvent {
                    sender: sender.clone(),
                    content: c,
                })
            }

            _ => unreachable!(),
        }
    }

    pub(crate) fn get_content_from_request(
        request: &OutgoingVerificationRequest,
    ) -> OutgoingContent {
        let request =
            if let OutgoingVerificationRequest::ToDevice(r) = request { r } else { unreachable!() };

        let json: Value = serde_json::from_str(
            request.messages.values().next().unwrap().values().next().unwrap().get(),
        )
        .unwrap();

        match request.event_type {
            EventType::KeyVerificationStart => {
                AnyToDeviceEventContent::KeyVerificationStart(serde_json::from_value(json).unwrap())
            }
            EventType::KeyVerificationKey => {
                AnyToDeviceEventContent::KeyVerificationKey(serde_json::from_value(json).unwrap())
            }
            EventType::KeyVerificationAccept => AnyToDeviceEventContent::KeyVerificationAccept(
                serde_json::from_value(json).unwrap(),
            ),
            EventType::KeyVerificationMac => {
                AnyToDeviceEventContent::KeyVerificationMac(serde_json::from_value(json).unwrap())
            }
            EventType::KeyVerificationCancel => AnyToDeviceEventContent::KeyVerificationCancel(
                serde_json::from_value(json).unwrap(),
            ),
            _ => unreachable!(),
        }
        .into()
    }
}
