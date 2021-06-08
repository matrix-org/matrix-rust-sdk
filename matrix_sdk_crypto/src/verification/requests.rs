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

use matrix_sdk_common::uuid::Uuid;
use ruma::{
    api::client::r0::to_device::DeviceIdOrAllDevices,
    events::{
        key::verification::{
            cancel::CancelCode,
            ready::{ReadyEventContent, ReadyToDeviceEventContent},
            request::RequestToDeviceEventContent,
            start::StartMethod,
            Relation, VerificationMethod,
        },
        room::message::KeyVerificationRequestEventContent,
        AnyMessageEventContent, AnyToDeviceEventContent,
    },
    DeviceId, DeviceIdBox, EventId, MilliSecondsSinceUnixEpoch, RoomId, UserId,
};
use tracing::{info, warn};

use super::{
    cache::VerificationCache,
    event_enums::{
        CancelContent, DoneContent, OutgoingContent, ReadyContent, RequestContent, StartContent,
    },
    sas::content_to_request,
    Cancelled, FlowId,
};
use crate::{
    olm::{PrivateCrossSigningIdentity, ReadOnlyAccount},
    store::CryptoStore,
    CryptoStoreError, OutgoingVerificationRequest, ReadOnlyDevice, RoomMessageRequest, Sas,
    ToDeviceRequest, UserIdentities,
};

const SUPPORTED_METHODS: &[VerificationMethod] = &[VerificationMethod::MSasV1];

#[derive(Clone, Debug)]
/// TODO
pub struct VerificationRequest {
    verification_cache: VerificationCache,
    account: ReadOnlyAccount,
    flow_id: Arc<FlowId>,
    other_user_id: Arc<UserId>,
    inner: Arc<Mutex<InnerRequest>>,
}

impl VerificationRequest {
    /// TODO
    pub(crate) fn new(
        cache: VerificationCache,
        account: ReadOnlyAccount,
        private_cross_signing_identity: PrivateCrossSigningIdentity,
        store: Arc<dyn CryptoStore>,
        room_id: &RoomId,
        event_id: &EventId,
        other_user: &UserId,
    ) -> Self {
        let flow_id = (room_id.to_owned(), event_id.to_owned()).into();

        let inner = Mutex::new(InnerRequest::Created(RequestState::new(
            account.clone(),
            private_cross_signing_identity,
            cache.clone(),
            store,
            other_user,
            &flow_id,
        )))
        .into();

        Self {
            account,
            verification_cache: cache,
            flow_id: flow_id.into(),
            inner,
            other_user_id: other_user.to_owned().into(),
        }
    }

    /// TODO
    pub(crate) fn new_to_device(
        cache: VerificationCache,
        account: ReadOnlyAccount,
        private_cross_signing_identity: PrivateCrossSigningIdentity,
        store: Arc<dyn CryptoStore>,
        other_user: &UserId,
    ) -> Self {
        let flow_id = Uuid::new_v4().to_string().into();

        let inner = Mutex::new(InnerRequest::Created(RequestState::new(
            account.clone(),
            private_cross_signing_identity,
            cache.clone(),
            store,
            other_user,
            &flow_id,
        )))
        .into();

        Self {
            account,
            verification_cache: cache,
            flow_id: flow_id.into(),
            inner,
            other_user_id: other_user.to_owned().into(),
        }
    }

    /// TODO
    pub fn request_to_device(&self) -> RequestToDeviceEventContent {
        RequestToDeviceEventContent::new(
            self.account.device_id().into(),
            self.flow_id().as_str().to_string(),
            SUPPORTED_METHODS.to_vec(),
            MilliSecondsSinceUnixEpoch::now(),
        )
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

    /// Has the verification flow that was started with this request finished.
    pub fn is_done(&self) -> bool {
        matches!(&*self.inner.lock().unwrap(), InnerRequest::Done(_))
    }

    /// Has the verification flow that was started with this request been
    /// cancelled.
    pub fn is_cancelled(&self) -> bool {
        matches!(&*self.inner.lock().unwrap(), InnerRequest::Cancelled(_))
    }

    pub(crate) fn from_request(
        cache: VerificationCache,
        account: ReadOnlyAccount,
        private_cross_signing_identity: PrivateCrossSigningIdentity,
        store: Arc<dyn CryptoStore>,
        sender: &UserId,
        flow_id: FlowId,
        content: &RequestContent,
    ) -> Self {
        Self {
            verification_cache: cache.clone(),
            inner: Arc::new(Mutex::new(InnerRequest::Requested(RequestState::from_request_event(
                account.clone(),
                private_cross_signing_identity,
                cache,
                store,
                sender,
                &flow_id,
                content,
            )))),
            account,
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
    pub(crate) fn receive_ready(&self, sender: &UserId, content: &ReadyContent) -> Result<(), ()> {
        let mut inner = self.inner.lock().unwrap();

        if let InnerRequest::Created(s) = &*inner {
            *inner = InnerRequest::Ready(s.clone().into_ready(sender, content));
        }

        Ok(())
    }

    pub(crate) async fn receive_start(
        &self,
        sender: &UserId,
        content: &StartContent<'_>,
    ) -> Result<(), CryptoStoreError> {
        let inner = self.inner.lock().unwrap().clone();

        if let InnerRequest::Ready(s) = inner {
            s.receive_start(sender, content).await?;
        } else {
            warn!(
                sender = sender.as_str(),
                device_id = content.from_device().as_str(),
                "Received a key verification start event but we're not yet in the ready state"
            )
        }

        Ok(())
    }

    pub(crate) fn receive_done(&self, sender: &UserId, content: &DoneContent<'_>) {
        if sender == self.other_user() {
            let mut inner = self.inner.lock().unwrap().clone();
            inner.receive_done(content);
        }
    }

    pub(crate) fn receive_cancel(&self, sender: &UserId, content: &CancelContent<'_>) {
        if sender == self.other_user() {
            let mut inner = self.inner.lock().unwrap().clone();
            inner.cancel(content.cancel_code());
        }
    }

    /// Is the verification request ready to start a verification flow.
    pub fn is_ready(&self) -> bool {
        matches!(&*self.inner.lock().unwrap(), InnerRequest::Ready(_))
    }

    pub(crate) fn start(
        &self,
        device: ReadOnlyDevice,
        user_identity: Option<UserIdentities>,
    ) -> Option<(Sas, OutgoingContent)> {
        match &*self.inner.lock().unwrap() {
            InnerRequest::Ready(s) => Some(s.clone().start_sas(
                s.store.clone(),
                s.account.clone(),
                s.private_cross_signing_identity.clone(),
                device,
                user_identity,
            )),
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

#[derive(Clone, Debug)]
enum InnerRequest {
    Created(RequestState<Created>),
    Requested(RequestState<Requested>),
    Ready(RequestState<Ready>),
    Passive(RequestState<Passive>),
    Done(RequestState<Done>),
    Cancelled(RequestState<Cancelled>),
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
            InnerRequest::Done(_) => DeviceIdOrAllDevices::AllDevices,
            InnerRequest::Cancelled(_) => DeviceIdOrAllDevices::AllDevices,
        }
    }

    fn other_user_id(&self) -> &UserId {
        match self {
            InnerRequest::Created(s) => &s.other_user_id,
            InnerRequest::Requested(s) => &s.other_user_id,
            InnerRequest::Ready(s) => &s.other_user_id,
            InnerRequest::Passive(s) => &s.other_user_id,
            InnerRequest::Done(s) => &s.other_user_id,
            InnerRequest::Cancelled(s) => &s.other_user_id,
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

    fn receive_done(&mut self, content: &DoneContent) {
        *self = InnerRequest::Done(match self {
            InnerRequest::Created(s) => s.clone().into_done(content),
            InnerRequest::Requested(s) => s.clone().into_done(content),
            InnerRequest::Ready(s) => s.clone().into_done(content),
            InnerRequest::Passive(s) => s.clone().into_done(content),
            InnerRequest::Done(s) => s.clone().into_done(content),
            InnerRequest::Cancelled(_) => return,
        })
    }

    fn cancel(&mut self, cancel_code: &CancelCode) {
        *self = InnerRequest::Cancelled(match self {
            InnerRequest::Created(s) => s.clone().into_canceled(cancel_code),
            InnerRequest::Requested(s) => s.clone().into_canceled(cancel_code),
            InnerRequest::Ready(s) => s.clone().into_canceled(cancel_code),
            InnerRequest::Passive(s) => s.clone().into_canceled(cancel_code),
            InnerRequest::Done(_) => return,
            InnerRequest::Cancelled(_) => return,
        })
    }

    fn to_started_sas(
        &self,
        content: &StartContent,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
    ) -> Result<Option<Sas>, OutgoingContent> {
        if let InnerRequest::Ready(s) = self {
            Ok(Some(s.to_started_sas(content, other_device, other_identity)?))
        } else {
            Ok(None)
        }
    }
}

#[derive(Clone, Debug)]
struct RequestState<S: Clone> {
    account: ReadOnlyAccount,
    private_cross_signing_identity: PrivateCrossSigningIdentity,
    verification_cache: VerificationCache,
    store: Arc<dyn CryptoStore>,
    flow_id: Arc<FlowId>,

    /// The id of the user which is participating in this verification request.
    pub other_user_id: UserId,

    /// The verification request state we are in.
    state: S,
}

impl<S: Clone> RequestState<S> {
    fn into_done(self, _: &DoneContent) -> RequestState<Done> {
        RequestState::<Done> {
            account: self.account,
            private_cross_signing_identity: self.private_cross_signing_identity,
            verification_cache: self.verification_cache,
            store: self.store,
            flow_id: self.flow_id,
            other_user_id: self.other_user_id,
            state: Done {},
        }
    }

    fn into_canceled(self, cancel_code: &CancelCode) -> RequestState<Cancelled> {
        RequestState::<Cancelled> {
            account: self.account,
            private_cross_signing_identity: self.private_cross_signing_identity,
            verification_cache: self.verification_cache,
            store: self.store,
            flow_id: self.flow_id,
            other_user_id: self.other_user_id,
            state: Cancelled::new(cancel_code.clone()),
        }
    }
}

impl RequestState<Created> {
    fn new(
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        cache: VerificationCache,
        store: Arc<dyn CryptoStore>,
        other_user_id: &UserId,
        flow_id: &FlowId,
    ) -> Self {
        Self {
            account,
            other_user_id: other_user_id.to_owned(),
            private_cross_signing_identity: private_identity,
            state: Created { methods: SUPPORTED_METHODS.to_vec(), flow_id: flow_id.to_owned() },
            verification_cache: cache,
            store,
            flow_id: flow_id.to_owned().into(),
        }
    }

    fn into_ready(self, _sender: &UserId, content: &ReadyContent) -> RequestState<Ready> {
        // TODO check the flow id, and that the methods match what we suggested.
        RequestState {
            account: self.account,
            flow_id: self.flow_id,
            verification_cache: self.verification_cache,
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
        cache: VerificationCache,
        store: Arc<dyn CryptoStore>,
        sender: &UserId,
        flow_id: &FlowId,
        content: &RequestContent,
    ) -> RequestState<Requested> {
        // TODO only create this if we support the methods
        RequestState {
            account,
            private_cross_signing_identity: private_identity,
            store,
            verification_cache: cache,
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
            verification_cache: self.verification_cache,
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
    fn to_started_sas<'a>(
        &self,
        content: &StartContent<'a>,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
    ) -> Result<Sas, OutgoingContent> {
        Sas::from_start_event(
            (&*self.flow_id).to_owned(),
            content,
            self.store.clone(),
            self.account.clone(),
            self.private_cross_signing_identity.clone(),
            other_device,
            other_identity,
            true,
        )
    }

    async fn receive_start(
        &self,
        sender: &UserId,
        content: &StartContent<'_>,
    ) -> Result<(), CryptoStoreError> {
        info!(
            sender = sender.as_str(),
            device = content.from_device().as_str(),
            "Received a new verification start event",
        );

        let device = if let Some(d) = self.store.get_device(sender, content.from_device()).await? {
            d
        } else {
            warn!(
                sender = sender.as_str(),
                device = content.from_device().as_str(),
                "Received a key verification start event from an unknown device",
            );

            return Ok(());
        };

        let identity = self.store.get_user_identity(sender).await?;

        match content.method() {
            StartMethod::SasV1(_) => match self.to_started_sas(content, device.clone(), identity) {
                Ok(s) => {
                    info!("Started a new SAS verification.");
                    self.verification_cache.insert_sas(s);
                }
                Err(c) => {
                    warn!(
                        user_id = device.user_id().as_str(),
                        device_id = device.device_id().as_str(),
                        content =? c,
                        "Can't start key verification, canceling.",
                    );
                    self.verification_cache.queue_up_content(
                        device.user_id(),
                        device.device_id(),
                        c,
                    )
                }
            },
            m => {
                warn!(method =? m, "Received a key verification start event with an unsupported method")
            }
        }

        Ok(())
    }

    fn start_sas(
        self,
        store: Arc<dyn CryptoStore>,
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
    ) -> (Sas, OutgoingContent) {
        match self.state.flow_id {
            FlowId::ToDevice(t) => {
                let (sas, content) = Sas::start(
                    account,
                    private_identity,
                    other_device,
                    store,
                    other_identity,
                    Some(t),
                );
                (sas, content)
            }
            FlowId::InRoom(r, e) => {
                let (sas, content) = Sas::start_in_room(
                    e,
                    r,
                    account,
                    private_identity,
                    other_device,
                    store,
                    other_identity,
                );
                (sas, content)
            }
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

#[derive(Clone, Debug)]
struct Done {}

#[cfg(test)]
mod test {
    use std::convert::TryFrom;

    use matrix_sdk_test::async_test;
    use ruma::{event_id, room_id, DeviceIdBox, UserId};

    use super::VerificationRequest;
    use crate::{
        olm::{PrivateCrossSigningIdentity, ReadOnlyAccount},
        store::{Changes, CryptoStore, MemoryStore},
        verification::{
            cache::VerificationCache,
            event_enums::{OutgoingContent, ReadyContent, StartContent},
            FlowId,
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
            VerificationCache::new(),
            bob,
            bob_identity,
            bob_store.into(),
            &room_id,
            &event_id,
            &alice_id(),
        );

        let flow_id = FlowId::from((room_id, event_id));

        let alice_request = VerificationRequest::from_request(
            VerificationCache::new(),
            alice,
            alice_identity,
            alice_store.into(),
            &bob_id(),
            flow_id,
            &(&content).into(),
        );

        let content: OutgoingContent = alice_request.accept().unwrap().into();
        let content = ReadyContent::try_from(&content).unwrap();

        bob_request.receive_ready(&alice_id(), &content).unwrap();

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

        let mut changes = Changes::default();
        changes.devices.new.push(bob_device.clone());
        alice_store.save_changes(changes).await.unwrap();

        let content = VerificationRequest::request(bob.user_id(), bob.device_id(), &alice_id());

        let bob_request = VerificationRequest::new(
            VerificationCache::new(),
            bob,
            bob_identity,
            bob_store.into(),
            &room_id,
            &event_id,
            &alice_id(),
        );

        let flow_id = FlowId::from((room_id, event_id));

        let alice_request = VerificationRequest::from_request(
            VerificationCache::new(),
            alice,
            alice_identity,
            alice_store.into(),
            &bob_id(),
            flow_id,
            &(&content).into(),
        );

        let content: OutgoingContent = alice_request.accept().unwrap().into();
        let content = ReadyContent::try_from(&content).unwrap();

        bob_request.receive_ready(&alice_id(), &content).unwrap();

        assert!(bob_request.is_ready());
        assert!(alice_request.is_ready());

        let (bob_sas, start_content) = bob_request.start(alice_device, None).unwrap();

        let content = StartContent::try_from(&start_content).unwrap();
        let flow_id = content.flow_id().to_owned();
        alice_request.receive_start(bob_device.user_id(), &content).await.unwrap();
        let alice_sas =
            alice_request.verification_cache.get_sas(bob_device.user_id(), &flow_id).unwrap();

        assert!(!bob_sas.is_cancelled());
        assert!(!alice_sas.is_cancelled());
    }

    #[async_test]
    async fn test_requesting_until_sas_to_device() {
        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let alice_device = ReadOnlyDevice::from_account(&alice).await;

        let alice_store: Box<dyn CryptoStore> = Box::new(MemoryStore::new());
        let alice_identity = PrivateCrossSigningIdentity::empty(alice_id());

        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());
        let bob_device = ReadOnlyDevice::from_account(&bob).await;
        let bob_store: Box<dyn CryptoStore> = Box::new(MemoryStore::new());
        let bob_identity = PrivateCrossSigningIdentity::empty(alice_id());

        let mut changes = Changes::default();
        changes.devices.new.push(bob_device.clone());
        alice_store.save_changes(changes).await.unwrap();

        let bob_request = VerificationRequest::new_to_device(
            VerificationCache::new(),
            bob,
            bob_identity,
            bob_store.into(),
            &alice_id(),
        );

        let content = bob_request.request_to_device();
        let flow_id = bob_request.flow_id().to_owned();

        let alice_request = VerificationRequest::from_request(
            VerificationCache::new(),
            alice,
            alice_identity,
            alice_store.into(),
            &bob_id(),
            flow_id,
            &(&content).into(),
        );

        let content: OutgoingContent = alice_request.accept().unwrap().into();
        let content = ReadyContent::try_from(&content).unwrap();

        bob_request.receive_ready(&alice_id(), &content).unwrap();

        assert!(bob_request.is_ready());
        assert!(alice_request.is_ready());

        let (bob_sas, start_content) = bob_request.start(alice_device, None).unwrap();

        let content = StartContent::try_from(&start_content).unwrap();
        let flow_id = content.flow_id().to_owned();
        alice_request.receive_start(bob_device.user_id(), &content).await.unwrap();
        let alice_sas =
            alice_request.verification_cache.get_sas(bob_device.user_id(), &flow_id).unwrap();

        assert!(!bob_sas.is_cancelled());
        assert!(!alice_sas.is_cancelled());
    }
}
