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

use std::sync::Arc;
#[cfg(test)]
use std::time::Instant;

use matrix_sdk_common::{
    events::{
        key::verification::{cancel::CancelCode, ShortAuthenticationString},
        AnyMessageEvent, AnyToDeviceEvent,
    },
    identifiers::{EventId, RoomId},
};

use super::{
    event_enums::{AcceptContent, CancelContent, MacContent, OutgoingContent},
    sas_state::{
        Accepted, Confirmed, Created, KeyReceived, MacReceived, SasState, Started, WaitingForDone,
    },
    FlowId, StartContent,
};
use crate::{
    identities::{ReadOnlyDevice, UserIdentities},
    verification::{Cancelled, Done},
    ReadOnlyAccount,
};

#[derive(Clone, Debug)]
pub enum InnerSas {
    Created(SasState<Created>),
    Started(SasState<Started>),
    Accepted(SasState<Accepted>),
    KeyReceived(SasState<KeyReceived>),
    Confirmed(SasState<Confirmed>),
    MacReceived(SasState<MacReceived>),
    WaitingForDone(SasState<WaitingForDone>),
    WaitingForDoneUnconfirmed(SasState<WaitingForDone>),
    Done(SasState<Done>),
    Cancelled(SasState<Cancelled>),
}

impl InnerSas {
    pub fn start(
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
        transaction_id: Option<String>,
    ) -> (InnerSas, StartContent) {
        let sas = SasState::<Created>::new(account, other_device, other_identity, transaction_id);
        let content = sas.as_content();
        (InnerSas::Created(sas), content)
    }

    pub fn supports_emoji(&self) -> bool {
        match self {
            InnerSas::Created(_) => false,
            InnerSas::Started(s) => s
                .state
                .accepted_protocols
                .short_auth_string
                .contains(&ShortAuthenticationString::Emoji),
            InnerSas::Accepted(s) => s
                .state
                .accepted_protocols
                .short_auth_string
                .contains(&ShortAuthenticationString::Emoji),
            InnerSas::KeyReceived(s) => s
                .state
                .accepted_protocols
                .short_auth_string
                .contains(&ShortAuthenticationString::Emoji),
            InnerSas::Confirmed(_) => false,
            InnerSas::MacReceived(s) => s
                .state
                .accepted_protocols
                .short_auth_string
                .contains(&ShortAuthenticationString::Emoji),
            InnerSas::WaitingForDone(_) => false,
            InnerSas::WaitingForDoneUnconfirmed(_) => false,
            InnerSas::Done(_) => false,
            InnerSas::Cancelled(_) => false,
        }
    }

    pub fn start_in_room(
        event_id: EventId,
        room_id: RoomId,
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
    ) -> (InnerSas, StartContent) {
        let sas = SasState::<Created>::new_in_room(
            room_id,
            event_id,
            account,
            other_device,
            other_identity,
        );
        let content = sas.as_content();
        (InnerSas::Created(sas), content)
    }

    pub fn from_start_event(
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        content: impl Into<StartContent>,
        other_identity: Option<UserIdentities>,
    ) -> Result<InnerSas, CancelContent> {
        match SasState::<Started>::from_start_event(account, other_device, other_identity, content)
        {
            Ok(s) => Ok(InnerSas::Started(s)),
            Err(s) => Err(s.as_content()),
        }
    }

    pub fn accept(&self) -> Option<AcceptContent> {
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
            InnerSas::Cancelled(s) => s.set_creation_time(time),
            InnerSas::Accepted(s) => s.set_creation_time(time),
            InnerSas::KeyReceived(s) => s.set_creation_time(time),
            InnerSas::Confirmed(s) => s.set_creation_time(time),
            InnerSas::MacReceived(s) => s.set_creation_time(time),
            InnerSas::Done(s) => s.set_creation_time(time),
            InnerSas::WaitingForDone(s) => s.set_creation_time(time),
            InnerSas::WaitingForDoneUnconfirmed(s) => s.set_creation_time(time),
        }
    }

    pub fn cancel(self, code: CancelCode) -> (InnerSas, Option<CancelContent>) {
        let sas = match self {
            InnerSas::Created(s) => s.cancel(code),
            InnerSas::Started(s) => s.cancel(code),
            InnerSas::Accepted(s) => s.cancel(code),
            InnerSas::KeyReceived(s) => s.cancel(code),
            InnerSas::MacReceived(s) => s.cancel(code),
            _ => return (self, None),
        };

        let content = sas.as_content();

        (InnerSas::Cancelled(sas), Some(content))
    }

    pub fn confirm(self) -> (InnerSas, Option<MacContent>) {
        match self {
            InnerSas::KeyReceived(s) => {
                let sas = s.confirm();
                let content = sas.as_content();
                (InnerSas::Confirmed(sas), Some(content))
            }
            InnerSas::MacReceived(s) => {
                if s.is_dm_verification() {
                    let sas = s.confirm_and_wait_for_done();
                    let content = sas.as_content();

                    (InnerSas::WaitingForDoneUnconfirmed(sas), Some(content))
                } else {
                    let sas = s.confirm();
                    let content = sas.as_content();

                    (InnerSas::Done(sas), Some(content))
                }
            }
            _ => (self, None),
        }
    }

    #[allow(dead_code)]
    pub fn receive_room_event(
        self,
        event: &AnyMessageEvent,
    ) -> (InnerSas, Option<OutgoingContent>) {
        match event {
            AnyMessageEvent::KeyVerificationKey(e) => match self {
                InnerSas::Accepted(s) => {
                    match s.into_key_received(&e.sender, (e.room_id.clone(), e.content.clone())) {
                        Ok(s) => (InnerSas::KeyReceived(s), None),
                        Err(s) => {
                            let content = s.as_content();
                            (InnerSas::Cancelled(s), Some(content.into()))
                        }
                    }
                }
                InnerSas::Started(s) => {
                    match s.into_key_received(&e.sender, (e.room_id.clone(), e.content.clone())) {
                        Ok(s) => {
                            let content = s.as_content();
                            (InnerSas::KeyReceived(s), Some(content.into()))
                        }
                        Err(s) => {
                            let content = s.as_content();
                            (InnerSas::Cancelled(s), Some(content.into()))
                        }
                    }
                }

                _ => (self, None),
            },
            AnyMessageEvent::KeyVerificationMac(e) => match self {
                InnerSas::KeyReceived(s) => {
                    match s.into_mac_received(&e.sender, (e.room_id.clone(), e.content.clone())) {
                        Ok(s) => (InnerSas::MacReceived(s), None),
                        Err(s) => {
                            let content = s.as_content();
                            (InnerSas::Cancelled(s), Some(content.into()))
                        }
                    }
                }
                InnerSas::Confirmed(s) => {
                    match s.into_waiting_for_done(&e.sender, (e.room_id.clone(), e.content.clone()))
                    {
                        Ok(s) => {
                            let content = s.done_content();
                            (InnerSas::WaitingForDone(s), Some(content.into()))
                        }
                        Err(s) => {
                            let content = s.as_content();
                            (InnerSas::Cancelled(s), Some(content.into()))
                        }
                    }
                }
                _ => (self, None),
            },
            AnyMessageEvent::KeyVerificationDone(e) => match self {
                InnerSas::WaitingForDone(s) => {
                    match s.into_done(&e.sender, (e.room_id.clone(), e.content.clone())) {
                        Ok(s) => (InnerSas::Done(s), None),
                        Err(s) => {
                            let content = s.as_content();
                            (InnerSas::Cancelled(s), Some(content.into()))
                        }
                    }
                }
                InnerSas::WaitingForDoneUnconfirmed(s) => {
                    match s.into_done(&e.sender, (e.room_id.clone(), e.content.clone())) {
                        Ok(s) => {
                            let content = s.done_content();
                            (InnerSas::Done(s), Some(content.into()))
                        }
                        Err(s) => {
                            let content = s.as_content();
                            (InnerSas::Cancelled(s), Some(content.into()))
                        }
                    }
                }

                _ => (self, None),
            },
            _ => (self, None),
        }
    }

    pub fn receive_event(self, event: &AnyToDeviceEvent) -> (InnerSas, Option<OutgoingContent>) {
        match event {
            AnyToDeviceEvent::KeyVerificationAccept(e) => {
                if let InnerSas::Created(s) = self {
                    match s.into_accepted(&e.sender, e.content.clone()) {
                        Ok(s) => {
                            let content = s.as_content();
                            (InnerSas::Accepted(s), Some(content.into()))
                        }
                        Err(s) => {
                            let content = s.as_content();
                            (InnerSas::Cancelled(s), Some(content.into()))
                        }
                    }
                } else {
                    (self, None)
                }
            }
            AnyToDeviceEvent::KeyVerificationKey(e) => match self {
                InnerSas::Accepted(s) => match s.into_key_received(&e.sender, e.content.clone()) {
                    Ok(s) => (InnerSas::KeyReceived(s), None),
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Cancelled(s), Some(content.into()))
                    }
                },
                InnerSas::Started(s) => match s.into_key_received(&e.sender, e.content.clone()) {
                    Ok(s) => {
                        let content = s.as_content();
                        (InnerSas::KeyReceived(s), Some(content.into()))
                    }
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Cancelled(s), Some(content.into()))
                    }
                },
                _ => (self, None),
            },
            AnyToDeviceEvent::KeyVerificationMac(e) => match self {
                InnerSas::KeyReceived(s) => {
                    match s.into_mac_received(&e.sender, e.content.clone()) {
                        Ok(s) => (InnerSas::MacReceived(s), None),
                        Err(s) => {
                            let content = s.as_content();
                            (InnerSas::Cancelled(s), Some(content.into()))
                        }
                    }
                }
                InnerSas::Confirmed(s) => match s.into_done(&e.sender, e.content.clone()) {
                    Ok(s) => (InnerSas::Done(s), None),
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Cancelled(s), Some(content.into()))
                    }
                },
                _ => (self, None),
            },
            _ => (self, None),
        }
    }

    pub fn can_be_presented(&self) -> bool {
        matches!(self, InnerSas::KeyReceived(_) | InnerSas::MacReceived(_))
    }

    pub fn is_done(&self) -> bool {
        matches!(self, InnerSas::Done(_))
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, InnerSas::Cancelled(_))
    }

    pub fn timed_out(&self) -> bool {
        match self {
            InnerSas::Created(s) => s.timed_out(),
            InnerSas::Started(s) => s.timed_out(),
            InnerSas::Cancelled(s) => s.timed_out(),
            InnerSas::Accepted(s) => s.timed_out(),
            InnerSas::KeyReceived(s) => s.timed_out(),
            InnerSas::Confirmed(s) => s.timed_out(),
            InnerSas::MacReceived(s) => s.timed_out(),
            InnerSas::WaitingForDone(s) => s.timed_out(),
            InnerSas::WaitingForDoneUnconfirmed(s) => s.timed_out(),
            InnerSas::Done(s) => s.timed_out(),
        }
    }

    pub fn verification_flow_id(&self) -> Arc<FlowId> {
        match self {
            InnerSas::Created(s) => s.verification_flow_id.clone(),
            InnerSas::Started(s) => s.verification_flow_id.clone(),
            InnerSas::Cancelled(s) => s.verification_flow_id.clone(),
            InnerSas::Accepted(s) => s.verification_flow_id.clone(),
            InnerSas::KeyReceived(s) => s.verification_flow_id.clone(),
            InnerSas::Confirmed(s) => s.verification_flow_id.clone(),
            InnerSas::MacReceived(s) => s.verification_flow_id.clone(),
            InnerSas::WaitingForDone(s) => s.verification_flow_id.clone(),
            InnerSas::WaitingForDoneUnconfirmed(s) => s.verification_flow_id.clone(),
            InnerSas::Done(s) => s.verification_flow_id.clone(),
        }
    }

    pub fn emoji(&self) -> Option<[(&'static str, &'static str); 7]> {
        match self {
            InnerSas::KeyReceived(s) => Some(s.get_emoji()),
            InnerSas::MacReceived(s) => Some(s.get_emoji()),
            _ => None,
        }
    }

    pub fn emoji_index(&self) -> Option<[u8; 7]> {
        match self {
            InnerSas::KeyReceived(s) => Some(s.get_emoji_index()),
            InnerSas::MacReceived(s) => Some(s.get_emoji_index()),
            _ => None,
        }
    }

    pub fn decimals(&self) -> Option<(u16, u16, u16)> {
        match self {
            InnerSas::KeyReceived(s) => Some(s.get_decimal()),
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
