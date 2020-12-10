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

#[cfg(test)]
use std::time::Instant;

use std::sync::Arc;

use matrix_sdk_common::events::{
    key::verification::{
        accept::AcceptToDeviceEventContent, cancel::CancelCode, mac::MacToDeviceEventContent,
        start::StartToDeviceEventContent,
    },
    AnyToDeviceEvent, AnyToDeviceEventContent, ToDeviceEvent,
};

use crate::{
    identities::{ReadOnlyDevice, UserIdentities},
    ReadOnlyAccount,
};

use super::sas_state::{
    Accepted, Canceled, Confirmed, Created, Done, KeyReceived, MacReceived, SasState, Started,
};

#[derive(Clone, Debug)]
pub enum InnerSas {
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
    pub fn start(
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
    ) -> (InnerSas, StartToDeviceEventContent) {
        let sas = SasState::<Created>::new(account, other_device, other_identity);
        let content = sas.as_content();
        (InnerSas::Created(sas), content)
    }

    pub fn from_start_event(
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        event: &ToDeviceEvent<StartToDeviceEventContent>,
        other_identity: Option<UserIdentities>,
    ) -> Result<InnerSas, AnyToDeviceEventContent> {
        match SasState::<Started>::from_start_event(account, other_device, event, other_identity) {
            Ok(s) => Ok(InnerSas::Started(s)),
            Err(s) => Err(s.as_content()),
        }
    }

    pub fn accept(&self) -> Option<AcceptToDeviceEventContent> {
        if let InnerSas::Started(s) = self {
            Some(s.as_content())
        } else {
            None
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn set_creation_time(&mut self, time: Instant) {
        match self {
            InnerSas::Created(s) => s.set_creation_time(time),
            InnerSas::Started(s) => s.set_creation_time(time),
            InnerSas::Canceled(s) => s.set_creation_time(time),
            InnerSas::Accepted(s) => s.set_creation_time(time),
            InnerSas::KeyRecieved(s) => s.set_creation_time(time),
            InnerSas::Confirmed(s) => s.set_creation_time(time),
            InnerSas::MacReceived(s) => s.set_creation_time(time),
            InnerSas::Done(s) => s.set_creation_time(time),
        }
    }

    pub fn cancel(self, code: CancelCode) -> (InnerSas, Option<AnyToDeviceEventContent>) {
        let sas = match self {
            InnerSas::Created(s) => s.cancel(code),
            InnerSas::Started(s) => s.cancel(code),
            InnerSas::Accepted(s) => s.cancel(code),
            InnerSas::KeyRecieved(s) => s.cancel(code),
            InnerSas::MacReceived(s) => s.cancel(code),
            _ => return (self, None),
        };

        let content = sas.as_content();

        (InnerSas::Canceled(sas), Some(content))
    }

    pub fn confirm(self) -> (InnerSas, Option<MacToDeviceEventContent>) {
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

    pub fn receive_event(
        self,
        event: &AnyToDeviceEvent,
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

    pub fn can_be_presented(&self) -> bool {
        match self {
            InnerSas::KeyRecieved(_) => true,
            InnerSas::MacReceived(_) => true,
            _ => false,
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(self, InnerSas::Done(_))
    }

    pub fn is_canceled(&self) -> bool {
        matches!(self, InnerSas::Canceled(_))
    }

    pub fn timed_out(&self) -> bool {
        match self {
            InnerSas::Created(s) => s.timed_out(),
            InnerSas::Started(s) => s.timed_out(),
            InnerSas::Canceled(s) => s.timed_out(),
            InnerSas::Accepted(s) => s.timed_out(),
            InnerSas::KeyRecieved(s) => s.timed_out(),
            InnerSas::Confirmed(s) => s.timed_out(),
            InnerSas::MacReceived(s) => s.timed_out(),
            InnerSas::Done(s) => s.timed_out(),
        }
    }

    pub fn verification_flow_id(&self) -> Arc<str> {
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

    pub fn emoji(&self) -> Option<Vec<(&'static str, &'static str)>> {
        match self {
            InnerSas::KeyRecieved(s) => Some(s.get_emoji()),
            InnerSas::MacReceived(s) => Some(s.get_emoji()),
            _ => None,
        }
    }

    pub fn decimals(&self) -> Option<(u16, u16, u16)> {
        match self {
            InnerSas::KeyRecieved(s) => Some(s.get_decimal()),
            InnerSas::MacReceived(s) => Some(s.get_decimal()),
            _ => None,
        }
    }

    pub fn verified_devices(&self) -> Option<Arc<[ReadOnlyDevice]>> {
        if let InnerSas::Done(s) = self {
            Some(s.verified_devices())
        } else {
            None
        }
    }

    pub fn verified_identities(&self) -> Option<Arc<[UserIdentities]>> {
        if let InnerSas::Done(s) = self {
            Some(s.verified_identities())
        } else {
            None
        }
    }
}
