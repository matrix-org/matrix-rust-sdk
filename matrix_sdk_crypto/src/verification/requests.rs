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

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use matrix_sdk_common::{
    api::r0::message::send_message_event::Response as RoomMessageResponse,
    events::{
        key::verification::{
            ready::ReadyEventContent, start::StartEventContent, Relation, VerificationMethod,
        },
        room::message::KeyVerificationRequestEventContent,
        MessageEvent,
    },
    identifiers::{DeviceId, DeviceIdBox, EventId, RoomId, UserId},
};

use crate::{
    olm::{PrivateCrossSigningIdentity, ReadOnlyAccount},
    store::CryptoStore,
    ReadOnlyDevice, Sas, UserIdentities, UserIdentity,
};

use super::sas::{OutgoingContent, StartContent};

const SUPPORTED_METHODS: &[VerificationMethod] = &[VerificationMethod::MSasV1];

#[derive(Clone, Debug)]
/// TODO
pub struct VerificationRequest {
    inner: Arc<Mutex<InnerRequest>>,
    account: ReadOnlyAccount,
    private_cross_signing_identity: PrivateCrossSigningIdentity,
    store: Arc<Box<dyn CryptoStore>>,
    room_id: Arc<RoomId>,
}

impl VerificationRequest {
    pub(crate) fn from_request_event(
        account: ReadOnlyAccount,
        private_cross_signing_identity: PrivateCrossSigningIdentity,
        store: Arc<Box<dyn CryptoStore>>,
        room_id: &RoomId,
        sender: &UserId,
        event_id: &EventId,
        content: &KeyVerificationRequestEventContent,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(InnerRequest::Requested(
                RequestState::from_request_event(
                    account.user_id(),
                    account.device_id(),
                    sender,
                    event_id,
                    content,
                ),
            ))),
            account,
            private_cross_signing_identity,
            store,
            room_id: room_id.clone().into(),
        }
    }

    /// The room id where the verification is happening.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Accept the verification request.
    pub fn accept(&self) -> Option<ReadyEventContent> {
        self.inner.lock().unwrap().accept()
    }

    pub(crate) fn into_started_sas(
        &self,
        event: &MessageEvent<StartEventContent>,
        device: ReadOnlyDevice,
        user_identity: Option<UserIdentities>,
    ) -> Result<Sas, OutgoingContent> {
        match &*self.inner.lock().unwrap() {
            InnerRequest::Ready(s) => s.into_started_sas(
                event,
                self.store.clone(),
                self.account.clone(),
                self.private_cross_signing_identity.clone(),
                device,
                user_identity,
            ),
            _ => todo!(),
        }
    }
}

#[derive(Debug)]
enum InnerRequest {
    Created(RequestState<Created>),
    Sent(RequestState<Sent>),
    Requested(RequestState<Requested>),
    Ready(RequestState<Ready>),
    Passive(RequestState<Passive>),
}

impl InnerRequest {
    fn accept(&mut self) -> Option<ReadyEventContent> {
        if let InnerRequest::Requested(s) = self {
            let (state, content) = s.clone().accept();
            *self = InnerRequest::Ready(state);

            Some(content)
        } else {
            None
        }
    }

    fn into_started_sas(
        &mut self,
        event: &MessageEvent<StartEventContent>,
        store: Arc<Box<dyn CryptoStore>>,
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
    ) -> Result<Option<Sas>, OutgoingContent> {
        if let InnerRequest::Ready(s) = self {
            Ok(Some(s.into_started_sas(
                event,
                store,
                account,
                private_identity,
                other_device,
                other_identity,
            )?))
        } else {
            Ok(None)
        }
    }
}

#[derive(Clone, Debug)]
struct RequestState<S: Clone> {
    /// Our own user id.
    pub own_user_id: UserId,

    /// Our own device id.
    pub own_device_id: DeviceIdBox,

    /// The id of the user which is participating in this verification request.
    pub other_user_id: UserId,

    /// The verification request state we are in.
    state: S,
}

#[derive(Clone, Debug)]
struct Created {}

impl RequestState<Created> {
    fn as_content(&self) -> KeyVerificationRequestEventContent {
        KeyVerificationRequestEventContent {
            body: format!(
                "{} is requesting to verify your key, but your client does not \
                support in-chat key verification. You will need to use legacy \
                key verification to verify keys.",
                self.own_user_id
            ),
            methods: SUPPORTED_METHODS.to_vec(),
            from_device: self.own_device_id.clone(),
            to: self.other_user_id.clone(),
        }
    }

    fn into_sent(self, response: &RoomMessageResponse) -> RequestState<Sent> {
        RequestState {
            own_user_id: self.own_user_id,
            own_device_id: self.own_device_id,
            other_user_id: self.other_user_id,
            state: Sent {
                methods: SUPPORTED_METHODS.to_vec(),
                flow_id: response.event_id.clone(),
            }
            .into(),
        }
    }
}

#[derive(Clone, Debug)]
struct Sent {
    /// The verification methods supported by the sender.
    pub methods: Vec<VerificationMethod>,

    /// The event id of our `m.key.verification.request` event which acts as an
    /// unique id identifying this verification flow.
    pub flow_id: EventId,
}

impl RequestState<Sent> {
    fn into_ready(self, _sender: &UserId, content: &ReadyEventContent) -> RequestState<Ready> {
        // TODO check the flow id, and that the methods match what we suggested.
        RequestState {
            own_user_id: self.own_user_id,
            own_device_id: self.own_device_id,
            other_user_id: self.other_user_id,
            state: Ready {
                methods: content.methods.to_owned(),
                other_device_id: content.from_device.clone(),
                flow_id: self.state.flow_id.clone(),
            }
            .into(),
        }
    }
}

#[derive(Clone, Debug)]
struct Requested {
    /// The verification methods supported by the sender.
    pub methods: Vec<VerificationMethod>,

    /// The event id of the `m.key.verification.request` event which acts as an
    /// unique id identifying this verification flow.
    pub flow_id: EventId,

    /// The device id of the device that responded to the verification request.
    pub other_device_id: DeviceIdBox,
}

impl RequestState<Requested> {
    fn from_request_event(
        own_user_id: &UserId,
        own_device_id: &DeviceId,
        sender: &UserId,
        event_id: &EventId,
        content: &KeyVerificationRequestEventContent,
    ) -> RequestState<Requested> {
        // TODO only create this if we suport the methods
        RequestState {
            own_user_id: own_user_id.clone(),
            own_device_id: own_device_id.into(),
            other_user_id: sender.clone(),
            state: Requested {
                methods: content.methods.clone(),
                flow_id: event_id.clone(),
                other_device_id: content.from_device.clone(),
            }
            .into(),
        }
    }

    fn accept(self) -> (RequestState<Ready>, ReadyEventContent) {
        let state = RequestState {
            own_user_id: self.own_user_id,
            own_device_id: self.own_device_id.clone(),
            other_user_id: self.other_user_id,
            state: Ready {
                methods: self.state.methods.clone(),
                other_device_id: self.state.other_device_id.clone(),
                flow_id: self.state.flow_id.clone(),
            },
        };

        let content = ReadyEventContent {
            from_device: self.own_device_id,
            methods: self.state.methods,
            relation: Relation {
                event_id: self.state.flow_id,
            },
        };

        (state, content)
    }
}

#[derive(Clone, Debug)]
struct Ready {
    /// The verification methods supported by the sender.
    pub methods: Vec<VerificationMethod>,

    /// The device id of the device that responded to the verification request.
    pub other_device_id: DeviceIdBox,

    /// The event id of the `m.key.verification.request` event which acts as an
    /// unique id identifying this verification flow.
    pub flow_id: EventId,
}

impl RequestState<Ready> {
    fn into_started_sas(
        &self,
        event: &MessageEvent<StartEventContent>,
        store: Arc<Box<dyn CryptoStore>>,
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
    ) -> Result<Sas, OutgoingContent> {
        Sas::from_start_event(
            account,
            private_identity,
            other_device,
            store,
            &event.sender,
            event.content.clone(),
            other_identity,
        )
    }

    fn start_sas(
        self,
        store: Arc<Box<dyn CryptoStore>>,
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
    ) -> (Sas, OutgoingContent) {
        Sas::start(
            account,
            private_identity,
            other_device,
            store,
            other_identity,
        )
    }
}

#[derive(Clone, Debug)]
struct Passive {
    /// The device id of the device that responded to the verification request.
    pub other_device_id: DeviceIdBox,

    /// The event id of the `m.key.verification.request` event which acts as an
    /// unique id identifying this verification flow.
    pub flow_id: EventId,
}

#[derive(Clone, Debug)]
struct Started {}
