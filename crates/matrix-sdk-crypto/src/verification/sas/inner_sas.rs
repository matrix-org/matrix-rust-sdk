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
use matrix_sdk_common::instant::Instant;
use ruma::{
    events::key::verification::{cancel::CancelCode, ShortAuthenticationString},
    EventId, RoomId, TransactionId, UserId,
};

use super::{
    sas_state::{
        Accepted, Confirmed, Created, KeyReceived, MacReceived, SasState, Started, WaitingForDone,
        WeAccepted,
    },
    FlowId,
};
use crate::{
    identities::{ReadOnlyDevice, ReadOnlyUserIdentities},
    verification::{
        event_enums::{AnyVerificationContent, OutgoingContent, OwnedAcceptContent, StartContent},
        Cancelled, Done,
    },
    Emoji, ReadOnlyAccount, ReadOnlyOwnUserIdentity,
};

#[derive(Clone, Debug)]
pub enum InnerSas {
    Created(SasState<Created>),
    Started(SasState<Started>),
    Accepted(SasState<Accepted>),
    WeAccepted(SasState<WeAccepted>),
    KeyReceived(SasState<KeyReceived>),
    Confirmed(SasState<Confirmed>),
    MacReceived(SasState<MacReceived>),
    WaitingForDone(SasState<WaitingForDone>),
    Done(SasState<Done>),
    Cancelled(SasState<Cancelled>),
}

impl InnerSas {
    pub fn start(
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        own_identity: Option<ReadOnlyOwnUserIdentity>,
        other_identity: Option<ReadOnlyUserIdentities>,
        transaction_id: Option<Box<TransactionId>>,
    ) -> (InnerSas, OutgoingContent) {
        let sas = SasState::<Created>::new(
            account,
            other_device,
            own_identity,
            other_identity,
            transaction_id,
        );
        let content = sas.as_content();
        (InnerSas::Created(sas), content.into())
    }

    pub fn started_from_request(&self) -> bool {
        match self {
            InnerSas::Created(s) => s.started_from_request,
            InnerSas::Started(s) => s.started_from_request,
            InnerSas::WeAccepted(s) => s.started_from_request,
            InnerSas::Accepted(s) => s.started_from_request,
            InnerSas::KeyReceived(s) => s.started_from_request,
            InnerSas::Confirmed(s) => s.started_from_request,
            InnerSas::MacReceived(s) => s.started_from_request,
            InnerSas::WaitingForDone(s) => s.started_from_request,
            InnerSas::Done(s) => s.started_from_request,
            InnerSas::Cancelled(s) => s.started_from_request,
        }
    }

    pub fn has_been_accepted(&self) -> bool {
        match self {
            InnerSas::Created(_) | InnerSas::Started(_) | InnerSas::Cancelled(_) => false,
            InnerSas::Accepted(_)
            | InnerSas::WeAccepted(_)
            | InnerSas::KeyReceived(_)
            | InnerSas::Confirmed(_)
            | InnerSas::MacReceived(_)
            | InnerSas::WaitingForDone(_)
            | InnerSas::Done(_) => true,
        }
    }

    pub fn supports_emoji(&self) -> bool {
        match self {
            InnerSas::Created(_) => false,
            InnerSas::Started(s) => s
                .state
                .accepted_protocols
                .short_auth_string
                .contains(&ShortAuthenticationString::Emoji),
            InnerSas::WeAccepted(s) => s
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
            InnerSas::Done(_) => false,
            InnerSas::Cancelled(_) => false,
        }
    }

    pub fn start_in_room(
        event_id: Box<EventId>,
        room_id: Box<RoomId>,
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        own_identity: Option<ReadOnlyOwnUserIdentity>,
        other_identity: Option<ReadOnlyUserIdentities>,
    ) -> (InnerSas, OutgoingContent) {
        let sas = SasState::<Created>::new_in_room(
            room_id,
            event_id,
            account,
            other_device,
            own_identity,
            other_identity,
        );
        let content = sas.as_content();
        (InnerSas::Created(sas), content.into())
    }

    pub fn from_start_event(
        account: ReadOnlyAccount,
        other_device: ReadOnlyDevice,
        flow_id: FlowId,
        content: &StartContent<'_>,
        own_identity: Option<ReadOnlyOwnUserIdentity>,
        other_identity: Option<ReadOnlyUserIdentities>,
        started_from_request: bool,
    ) -> Result<InnerSas, OutgoingContent> {
        match SasState::<Started>::from_start_event(
            account,
            other_device,
            own_identity,
            other_identity,
            flow_id,
            content,
            started_from_request,
        ) {
            Ok(s) => Ok(InnerSas::Started(s)),
            Err(s) => Err(s.as_content()),
        }
    }

    pub fn accept(
        self,
        methods: Vec<ShortAuthenticationString>,
    ) -> Option<(InnerSas, OwnedAcceptContent)> {
        if let InnerSas::Started(s) = self {
            let sas = s.into_we_accepted(methods);
            let content = sas.as_content();
            Some((InnerSas::WeAccepted(sas), content))
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
            InnerSas::WeAccepted(s) => s.set_creation_time(time),
        }
    }

    pub fn cancel(
        self,
        cancelled_by_us: bool,
        code: CancelCode,
    ) -> (InnerSas, Option<OutgoingContent>) {
        let sas = match self {
            InnerSas::Created(s) => s.cancel(cancelled_by_us, code),
            InnerSas::Started(s) => s.cancel(cancelled_by_us, code),
            InnerSas::Accepted(s) => s.cancel(cancelled_by_us, code),
            InnerSas::WeAccepted(s) => s.cancel(cancelled_by_us, code),
            InnerSas::KeyReceived(s) => s.cancel(cancelled_by_us, code),
            InnerSas::MacReceived(s) => s.cancel(cancelled_by_us, code),
            InnerSas::Confirmed(s) => s.cancel(cancelled_by_us, code),
            InnerSas::WaitingForDone(s) => s.cancel(cancelled_by_us, code),
            InnerSas::Done(_) | InnerSas::Cancelled(_) => return (self, None),
        };

        let content = sas.as_content();

        (InnerSas::Cancelled(sas), Some(content))
    }

    pub fn confirm(self) -> (InnerSas, Vec<OutgoingContent>) {
        match self {
            InnerSas::KeyReceived(s) => {
                let sas = s.confirm();
                let content = sas.as_content();
                (InnerSas::Confirmed(sas), vec![content])
            }
            InnerSas::MacReceived(s) => {
                if s.started_from_request {
                    let sas = s.confirm_and_wait_for_done();
                    let contents = vec![sas.as_content(), sas.done_content()];

                    (InnerSas::WaitingForDone(sas), contents)
                } else {
                    let sas = s.confirm();
                    let content = sas.as_content();

                    (InnerSas::Done(sas), vec![content])
                }
            }
            _ => (self, Vec::new()),
        }
    }

    pub fn receive_any_event(
        self,
        sender: &UserId,
        content: &AnyVerificationContent<'_>,
    ) -> (Self, Option<OutgoingContent>) {
        match content {
            AnyVerificationContent::Accept(c) => match self {
                InnerSas::Created(s) => match s.into_accepted(sender, c) {
                    Ok(s) => {
                        let content = s.as_content();
                        (InnerSas::Accepted(s), Some(content))
                    }
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Cancelled(s), Some(content))
                    }
                },
                InnerSas::Started(s) => match s.into_accepted(sender, c) {
                    Ok(s) => {
                        let content = s.as_content();
                        (InnerSas::Accepted(s), Some(content))
                    }
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Cancelled(s), Some(content))
                    }
                },
                _ => (self, None),
            },
            AnyVerificationContent::Cancel(c) => {
                let (sas, _) = self.cancel(false, c.cancel_code().to_owned());
                (sas, None)
            }
            AnyVerificationContent::Key(c) => match self {
                InnerSas::Accepted(s) => match s.into_key_received(sender, c) {
                    Ok(s) => (InnerSas::KeyReceived(s), None),
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Cancelled(s), Some(content))
                    }
                },
                InnerSas::WeAccepted(s) => match s.into_key_received(sender, c) {
                    Ok(s) => {
                        let content = s.as_content();
                        (InnerSas::KeyReceived(s), Some(content))
                    }
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Cancelled(s), Some(content))
                    }
                },

                _ => (self, None),
            },
            AnyVerificationContent::Mac(c) => match self {
                InnerSas::KeyReceived(s) => match s.into_mac_received(sender, c) {
                    Ok(s) => (InnerSas::MacReceived(s), None),
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Cancelled(s), Some(content))
                    }
                },
                InnerSas::Confirmed(s) =>
                // TODO remove the else branch when we remove the ability to
                // start from a `m.key.verification.start` event.
                {
                    match if s.started_from_request {
                        s.into_waiting_for_done(sender, c)
                            .map(|s| (Some(s.done_content()), InnerSas::WaitingForDone(s)))
                    } else {
                        s.into_done(sender, c).map(|s| (None, InnerSas::Done(s)))
                    } {
                        Ok((c, s)) => (s, c),
                        Err(s) => {
                            let content = s.as_content();
                            (InnerSas::Cancelled(s), Some(content))
                        }
                    }
                }
                _ => (self, None),
            },
            AnyVerificationContent::Done(c) => match self {
                InnerSas::WaitingForDone(s) => match s.into_done(sender, c) {
                    Ok(s) => (InnerSas::Done(s), None),
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Cancelled(s), Some(content))
                    }
                },
                _ => (self, None),
            },
            AnyVerificationContent::Request(_)
            | AnyVerificationContent::Ready(_)
            | AnyVerificationContent::Start(_) => (self, None),
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

    pub fn have_we_confirmed(&self) -> bool {
        matches!(self, InnerSas::Confirmed(_) | InnerSas::WaitingForDone(_) | InnerSas::Done(_))
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
            InnerSas::Done(s) => s.timed_out(),
            InnerSas::WeAccepted(s) => s.timed_out(),
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
            InnerSas::Done(s) => s.verification_flow_id.clone(),
            InnerSas::WeAccepted(s) => s.verification_flow_id.clone(),
        }
    }

    pub fn emoji(&self) -> Option<[Emoji; 7]> {
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

    pub fn verified_identities(&self) -> Option<Arc<[ReadOnlyUserIdentities]>> {
        if let InnerSas::Done(s) = self {
            Some(s.verified_identities())
        } else {
            None
        }
    }
}
