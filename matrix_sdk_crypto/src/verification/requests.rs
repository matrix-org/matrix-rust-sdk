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

use std::{
    convert::TryFrom,
    sync::{Arc, Mutex},
};

use matrix_sdk_common::{
    api::r0::to_device::DeviceIdOrAllDevices,
    events::{
        key::verification::{
            ready::{ReadyEventContent, ReadyToDeviceEventContent},
            request::RequestToDeviceEventContent,
            start::StartEventContent,
            Relation, VerificationMethod,
        },
        room::message::KeyVerificationRequestEventContent,
        AnyMessageEventContent, AnyToDeviceEventContent, MessageEvent, SyncMessageEvent,
    },
    identifiers::{DeviceId, DeviceIdBox, EventId, RoomId, UserId},
    uuid::Uuid,
};

use super::{
    sas::{content_to_request, OutgoingContent, StartContent},
    FlowId,
};
use crate::{
    olm::{PrivateCrossSigningIdentity, ReadOnlyAccount},
    store::CryptoStore,
    OutgoingVerificationRequest, ReadOnlyDevice, RoomMessageRequest, Sas, ToDeviceRequest,
    UserIdentities,
};

const SUPPORTED_METHODS: &[VerificationMethod] = &[VerificationMethod::MSasV1];

pub enum RequestContent<'a> {
    ToDevice(&'a RequestToDeviceEventContent),
    Room(&'a KeyVerificationRequestEventContent),
}

impl RequestContent<'_> {
    fn from_device(&self) -> &DeviceId {
        match self {
            RequestContent::ToDevice(t) => &t.from_device,
            RequestContent::Room(r) => &r.from_device,
        }
    }

    fn methods(&self) -> &[VerificationMethod] {
        match self {
            RequestContent::ToDevice(t) => &t.methods,
            RequestContent::Room(r) => &r.methods,
        }
    }
}

impl<'a> From<&'a KeyVerificationRequestEventContent> for RequestContent<'a> {
    fn from(c: &'a KeyVerificationRequestEventContent) -> Self {
        Self::Room(c)
    }
}

impl<'a> From<&'a RequestToDeviceEventContent> for RequestContent<'a> {
    fn from(c: &'a RequestToDeviceEventContent) -> Self {
        Self::ToDevice(c)
    }
}

pub enum ReadyContent<'a> {
    ToDevice(&'a ReadyToDeviceEventContent),
    Room(&'a ReadyEventContent),
}

impl ReadyContent<'_> {
    fn from_device(&self) -> &DeviceId {
        match self {
            ReadyContent::ToDevice(t) => &t.from_device,
            ReadyContent::Room(r) => &r.from_device,
        }
    }

    fn methods(&self) -> &[VerificationMethod] {
        match self {
            ReadyContent::ToDevice(t) => &t.methods,
            ReadyContent::Room(r) => &r.methods,
        }
    }
}

impl<'a> From<&'a ReadyEventContent> for ReadyContent<'a> {
    fn from(c: &'a ReadyEventContent) -> Self {
        Self::Room(c)
    }
}

impl<'a> From<&'a ReadyToDeviceEventContent> for ReadyContent<'a> {
    fn from(c: &'a ReadyToDeviceEventContent) -> Self {
        Self::ToDevice(c)
    }
}

impl<'a> TryFrom<&'a OutgoingContent> for ReadyContent<'a> {
    type Error = ();

    fn try_from(value: &'a OutgoingContent) -> Result<Self, Self::Error> {
        match value {
            OutgoingContent::Room(_, c) => {
                if let AnyMessageEventContent::KeyVerificationReady(c) = c {
                    Ok(ReadyContent::Room(c))
                } else {
                    Err(())
                }
            }
            OutgoingContent::ToDevice(c) => {
                if let AnyToDeviceEventContent::KeyVerificationReady(c) = c {
                    Ok(ReadyContent::ToDevice(c))
                } else {
                    Err(())
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
/// TODO
pub struct VerificationRequest {
    flow_id: Arc<FlowId>,
    other_user_id: Arc<UserId>,
    inner: Arc<Mutex<InnerRequest>>,
}

impl VerificationRequest {
    /// TODO
    pub fn new(
        account: ReadOnlyAccount,
        private_cross_signing_identity: PrivateCrossSigningIdentity,
        store: Arc<Box<dyn CryptoStore>>,
        room_id: &RoomId,
        event_id: &EventId,
        other_user: &UserId,
    ) -> Self {
        let flow_id = (room_id.to_owned(), event_id.to_owned()).into();

        let inner = Mutex::new(InnerRequest::Created(RequestState::new(
            account,
            private_cross_signing_identity,
            store,
            other_user,
            &flow_id,
        )))
        .into();

        Self { flow_id: flow_id.into(), inner, other_user_id: other_user.to_owned().into() }
    }

    /// TODO
    pub fn request(
        own_user_id: &UserId,
        own_device_id: &DeviceId,
        other_user_id: &UserId,
    ) -> KeyVerificationRequestEventContent {
        KeyVerificationRequestEventContent::new(
            format!(
                "{} is requesting to verify your key, but your client does not \
                support in-chat key verification. You will need to use legacy \
                key verification to verify keys.",
                own_user_id
            ),
            SUPPORTED_METHODS.to_vec(),
            own_device_id.into(),
            other_user_id.to_owned(),
        )
    }

    /// The id of the other user that is participating in this verification
    /// request.
    pub fn other_user(&self) -> &UserId {
        &self.other_user_id
    }

    /// Get the unique ID of this verification request
    pub fn flow_id(&self) -> &FlowId {
        &self.flow_id
    }

    pub(crate) fn from_room_request(
        account: ReadOnlyAccount,
        private_cross_signing_identity: PrivateCrossSigningIdentity,
        store: Arc<Box<dyn CryptoStore>>,
        sender: &UserId,
        event_id: &EventId,
        room_id: &RoomId,
        content: &KeyVerificationRequestEventContent,
    ) -> Self {
        let flow_id = FlowId::from((room_id.to_owned(), event_id.to_owned()));
        Self::from_helper(
            account,
            private_cross_signing_identity,
            store,
            sender,
            flow_id,
            content.into(),
        )
    }

    pub(crate) fn from_request(
        account: ReadOnlyAccount,
        private_cross_signing_identity: PrivateCrossSigningIdentity,
        store: Arc<Box<dyn CryptoStore>>,
        sender: &UserId,
        content: &RequestToDeviceEventContent,
    ) -> Self {
        let flow_id = FlowId::from(content.transaction_id.to_owned());
        Self::from_helper(
            account,
            private_cross_signing_identity,
            store,
            sender,
            flow_id,
            content.into(),
        )
    }

    fn from_helper(
        account: ReadOnlyAccount,
        private_cross_signing_identity: PrivateCrossSigningIdentity,
        store: Arc<Box<dyn CryptoStore>>,
        sender: &UserId,
        flow_id: FlowId,
        content: RequestContent,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(InnerRequest::Requested(RequestState::from_request_event(
                account,
                private_cross_signing_identity,
                store,
                sender,
                &flow_id,
                content,
            )))),
            other_user_id: sender.to_owned().into(),
            flow_id: flow_id.into(),
        }
    }

    /// Accept the verification request.
    pub fn accept(&self) -> Option<OutgoingVerificationRequest> {
        let mut inner = self.inner.lock().unwrap();

        inner.accept().map(|c| match c {
            OutgoingContent::ToDevice(content) => {
                self.content_to_request(inner.other_device_id(), content).into()
            }
            OutgoingContent::Room(room_id, content) => {
                RoomMessageRequest { room_id, txn_id: Uuid::new_v4(), content }.into()
            }
        })
    }

    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn receive_ready<'a>(
        &self,
        sender: &UserId,
        content: impl Into<ReadyContent<'a>>,
    ) -> Result<(), ()> {
        let mut inner = self.inner.lock().unwrap();
        let content = content.into();

        if let InnerRequest::Created(s) = &*inner {
            *inner = InnerRequest::Ready(s.clone().into_ready(sender, content));
        }

        Ok(())
    }

    /// Is the verification request ready to start a verification flow.
    pub fn is_ready(&self) -> bool {
        matches!(&*self.inner.lock().unwrap(), InnerRequest::Ready(_))
    }

    pub(crate) fn into_started_sas(
        self,
        event: &SyncMessageEvent<StartEventContent>,
        device: ReadOnlyDevice,
        user_identity: Option<UserIdentities>,
    ) -> Result<Sas, OutgoingContent> {
        match &*self.inner.lock().unwrap() {
            InnerRequest::Ready(s) => match &s.state.flow_id {
                FlowId::ToDevice(_) => todo!(),
                FlowId::InRoom(r, _) => s.clone().into_started_sas(
                    &event.clone().into_full_event(r.to_owned()),
                    s.store.clone(),
                    s.account.clone(),
                    s.private_cross_signing_identity.clone(),
                    device,
                    user_identity,
                ),
            },
            // TODO cancel here since we got a missmatched message or do
            // nothing?
            _ => todo!(),
        }
    }

    pub(crate) fn start(
        &self,
        device: ReadOnlyDevice,
        user_identity: Option<UserIdentities>,
    ) -> Option<(Sas, StartContent)> {
        match &*self.inner.lock().unwrap() {
            InnerRequest::Ready(s) => match &s.state.flow_id {
                FlowId::ToDevice(_) => todo!(),
                FlowId::InRoom(_, _) => Some(s.clone().start_sas(
                    s.store.clone(),
                    s.account.clone(),
                    s.private_cross_signing_identity.clone(),
                    device,
                    user_identity,
                )),
            },
            _ => None,
        }
    }

    fn content_to_request(
        &self,
        other_device_id: DeviceIdOrAllDevices,
        content: AnyToDeviceEventContent,
    ) -> ToDeviceRequest {
        content_to_request(&self.other_user_id, other_device_id, content)
    }
}

#[derive(Debug)]
enum InnerRequest {
    Created(RequestState<Created>),
    Requested(RequestState<Requested>),
    Ready(RequestState<Ready>),
    Passive(RequestState<Passive>),
}

impl InnerRequest {
    fn other_device_id(&self) -> DeviceIdOrAllDevices {
        match self {
            InnerRequest::Created(_) => DeviceIdOrAllDevices::AllDevices,
            InnerRequest::Requested(_) => DeviceIdOrAllDevices::AllDevices,
            InnerRequest::Ready(r) => {
                DeviceIdOrAllDevices::DeviceId(r.state.other_device_id.to_owned())
            }
            InnerRequest::Passive(_) => DeviceIdOrAllDevices::AllDevices,
        }
    }

    fn other_user_id(&self) -> &UserId {
        match self {
            InnerRequest::Created(s) => &s.other_user_id,
            InnerRequest::Requested(s) => &s.other_user_id,
            InnerRequest::Ready(s) => &s.other_user_id,
            InnerRequest::Passive(s) => &s.other_user_id,
        }
    }

    fn accept(&mut self) -> Option<OutgoingContent> {
        if let InnerRequest::Requested(s) = self {
            let (state, content) = s.clone().accept();
            *self = InnerRequest::Ready(state);

            Some(content)
        } else {
            None
        }
    }

    fn into_started_sas(
        self,
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
    account: ReadOnlyAccount,
    private_cross_signing_identity: PrivateCrossSigningIdentity,
    store: Arc<Box<dyn CryptoStore>>,
    flow_id: Arc<FlowId>,

    /// The id of the user which is participating in this verification request.
    pub other_user_id: UserId,

    /// The verification request state we are in.
    state: S,
}

impl RequestState<Created> {
    fn new(
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        store: Arc<Box<dyn CryptoStore>>,
        other_user_id: &UserId,
        flow_id: &FlowId,
    ) -> Self {
        Self {
            account,
            other_user_id: other_user_id.to_owned(),
            private_cross_signing_identity: private_identity,
            state: Created { methods: SUPPORTED_METHODS.to_vec(), flow_id: flow_id.to_owned() },
            store,
            flow_id: flow_id.to_owned().into(),
        }
    }

    fn into_ready(self, _sender: &UserId, content: ReadyContent) -> RequestState<Ready> {
        // TODO check the flow id, and that the methods match what we suggested.
        RequestState {
            account: self.account,
            flow_id: self.flow_id,
            private_cross_signing_identity: self.private_cross_signing_identity,
            store: self.store,
            other_user_id: self.other_user_id,
            state: Ready {
                methods: content.methods().to_owned(),
                other_device_id: content.from_device().into(),
                flow_id: self.state.flow_id,
            },
        }
    }
}

#[derive(Clone, Debug)]
struct Created {
    /// The verification methods supported by the sender.
    pub methods: Vec<VerificationMethod>,

    /// The event id of our `m.key.verification.request` event which acts as an
    /// unique id identifying this verification flow.
    pub flow_id: FlowId,
}

#[derive(Clone, Debug)]
struct Requested {
    /// The verification methods supported by the sender.
    pub methods: Vec<VerificationMethod>,

    /// The event id of the `m.key.verification.request` event which acts as an
    /// unique id identifying this verification flow.
    pub flow_id: FlowId,

    /// The device id of the device that responded to the verification request.
    pub other_device_id: DeviceIdBox,
}

impl RequestState<Requested> {
    fn from_request_event(
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        store: Arc<Box<dyn CryptoStore>>,
        sender: &UserId,
        flow_id: &FlowId,
        content: RequestContent,
    ) -> RequestState<Requested> {
        // TODO only create this if we suport the methods
        RequestState {
            account,
            private_cross_signing_identity: private_identity,
            store,
            flow_id: flow_id.to_owned().into(),
            other_user_id: sender.clone(),
            state: Requested {
                methods: content.methods().to_owned(),
                flow_id: flow_id.clone(),
                other_device_id: content.from_device().into(),
            },
        }
    }

    fn accept(self) -> (RequestState<Ready>, OutgoingContent) {
        let state = RequestState {
            account: self.account.clone(),
            store: self.store,
            private_cross_signing_identity: self.private_cross_signing_identity,
            flow_id: self.flow_id,
            other_user_id: self.other_user_id,
            state: Ready {
                methods: SUPPORTED_METHODS.to_vec(),
                other_device_id: self.state.other_device_id.clone(),
                flow_id: self.state.flow_id.clone(),
            },
        };

        let content = match self.state.flow_id {
            FlowId::ToDevice(i) => {
                AnyToDeviceEventContent::KeyVerificationReady(ReadyToDeviceEventContent::new(
                    self.account.device_id().to_owned(),
                    SUPPORTED_METHODS.to_vec(),
                    i,
                ))
                .into()
            }
            FlowId::InRoom(r, e) => (
                r,
                AnyMessageEventContent::KeyVerificationReady(ReadyEventContent::new(
                    self.account.device_id().to_owned(),
                    SUPPORTED_METHODS.to_vec(),
                    Relation::new(e),
                )),
            )
                .into(),
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
    pub flow_id: FlowId,
}

impl RequestState<Ready> {
    fn into_started_sas(
        self,
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
            (event.room_id.clone(), event.content.clone()),
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
    ) -> (Sas, StartContent) {
        match self.state.flow_id {
            FlowId::ToDevice(t) => {
                Sas::start(account, private_identity, other_device, store, other_identity, Some(t))
            }
            FlowId::InRoom(r, e) => Sas::start_in_room(
                e,
                r,
                account,
                private_identity,
                other_device,
                store,
                other_identity,
            ),
        }
    }
}

#[derive(Clone, Debug)]
struct Passive {
    /// The device id of the device that responded to the verification request.
    pub other_device_id: DeviceIdBox,

    /// The event id of the `m.key.verification.request` event which acts as an
    /// unique id identifying this verification flow.
    pub flow_id: FlowId,
}

#[cfg(test)]
mod test {
    use std::convert::TryFrom;

    use matrix_sdk_common::{
        events::{SyncMessageEvent, Unsigned},
        identifiers::{event_id, room_id, DeviceIdBox, UserId},
        MilliSecondsSinceUnixEpoch,
    };
    use matrix_sdk_test::async_test;

    use super::VerificationRequest;
    use crate::{
        olm::{PrivateCrossSigningIdentity, ReadOnlyAccount},
        store::{CryptoStore, MemoryStore},
        verification::{
            requests::ReadyContent,
            sas::{OutgoingContent, StartContent},
        },
        ReadOnlyDevice,
    };

    fn alice_id() -> UserId {
        UserId::try_from("@alice:example.org").unwrap()
    }

    fn alice_device_id() -> DeviceIdBox {
        "JLAFKJWSCS".into()
    }

    fn bob_id() -> UserId {
        UserId::try_from("@bob:example.org").unwrap()
    }

    fn bob_device_id() -> DeviceIdBox {
        "BOBDEVCIE".into()
    }

    #[async_test]
    async fn test_request_accepting() {
        let event_id = event_id!("$1234localhost");
        let room_id = room_id!("!test:localhost");

        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let alice_store: Box<dyn CryptoStore> = Box::new(MemoryStore::new());
        let alice_identity = PrivateCrossSigningIdentity::empty(alice_id());

        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());
        let bob_store: Box<dyn CryptoStore> = Box::new(MemoryStore::new());
        let bob_identity = PrivateCrossSigningIdentity::empty(alice_id());

        let content = VerificationRequest::request(bob.user_id(), bob.device_id(), &alice_id());

        let bob_request = VerificationRequest::new(
            bob,
            bob_identity,
            bob_store.into(),
            &room_id,
            &event_id,
            &alice_id(),
        );

        let alice_request = VerificationRequest::from_room_request(
            alice,
            alice_identity,
            alice_store.into(),
            &bob_id(),
            &event_id,
            &room_id,
            &content,
        );

        let content: OutgoingContent = alice_request.accept().unwrap().into();
        let content = ReadyContent::try_from(&content).unwrap();

        bob_request.receive_ready(&alice_id(), content).unwrap();

        assert!(bob_request.is_ready());
        assert!(alice_request.is_ready());
    }

    #[async_test]
    async fn test_requesting_until_sas() {
        let event_id = event_id!("$1234localhost");
        let room_id = room_id!("!test:localhost");

        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let alice_device = ReadOnlyDevice::from_account(&alice).await;

        let alice_store: Box<dyn CryptoStore> = Box::new(MemoryStore::new());
        let alice_identity = PrivateCrossSigningIdentity::empty(alice_id());

        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());
        let bob_device = ReadOnlyDevice::from_account(&bob).await;
        let bob_store: Box<dyn CryptoStore> = Box::new(MemoryStore::new());
        let bob_identity = PrivateCrossSigningIdentity::empty(alice_id());

        let content = VerificationRequest::request(bob.user_id(), bob.device_id(), &alice_id());

        let bob_request = VerificationRequest::new(
            bob,
            bob_identity,
            bob_store.into(),
            &room_id,
            &event_id,
            &alice_id(),
        );

        let alice_request = VerificationRequest::from_room_request(
            alice,
            alice_identity,
            alice_store.into(),
            &bob_id(),
            &event_id,
            &room_id,
            &content,
        );

        let content: OutgoingContent = alice_request.accept().unwrap().into();
        let content = ReadyContent::try_from(&content).unwrap();

        bob_request.receive_ready(&alice_id(), content).unwrap();

        assert!(bob_request.is_ready());
        assert!(alice_request.is_ready());

        let (bob_sas, start_content) = bob_request.start(alice_device, None).unwrap();

        let event = if let StartContent::Room(_, c) = start_content {
            SyncMessageEvent {
                content: c,
                event_id: event_id.clone(),
                sender: bob_id(),
                origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
                unsigned: Unsigned::default(),
            }
        } else {
            panic!("Invalid start event content type");
        };

        let alice_sas = alice_request.into_started_sas(&event, bob_device, None).unwrap();

        assert!(!bob_sas.is_canceled());
        assert!(!alice_sas.is_canceled());
    }
}
