/// TODO
///
/// Make a state machine that handles key requests.
///
/// Start with the outgoing key requests. We need to queue up a request and
/// store the outgoing key requests, store if the request was sent out.
/// Once we receive a key, check if we have an outgoing requests and if so
/// accept and store the key if we don't have a better one. Send out a
/// key request cancelation if we receive one.
///
/// Incoming key requests:
/// First handle the easy case, if we trust the device and have a session, queue
/// up a to-device request.
///
/// If we don't have a session, queue up a key claim request, once we get a
/// session send out the key if we trust the device.
///
/// If we don't trust the device store an object that remembers the request and
/// let the users introspect that object.
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::value::to_raw_value;
use std::{collections::BTreeMap, sync::Arc};
use tracing::{error, info, trace};

use matrix_sdk_common::{
    api::r0::to_device::DeviceIdOrAllDevices,
    events::{
        forwarded_room_key::ForwardedRoomKeyEventContent,
        room_key_request::{Action, RequestedKeyInfo, RoomKeyRequestEventContent},
        EventType, ToDeviceEvent,
    },
    identifiers::{DeviceIdBox, EventEncryptionAlgorithm, RoomId, UserId},
    uuid::Uuid,
};

use crate::{
    requests::{OutgoingRequest, ToDeviceRequest},
    store::Store,
};

#[derive(Debug, Clone)]
struct KeyRequestMachine {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceIdBox>,
    store: Store,
    outgoing_to_device_requests: Arc<DashMap<Uuid, OutgoingRequest>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OugoingKeyInfo {
    request_id: Uuid,
    info: RequestedKeyInfo,
    sent_out: bool,
}

trait Encode {
    fn encode(&self) -> String;
}

impl Encode for RequestedKeyInfo {
    fn encode(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.sender_key, self.room_id, self.session_id, self.algorithm
        )
    }
}

impl Encode for ForwardedRoomKeyEventContent {
    fn encode(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.sender_key, self.room_id, self.session_id, self.algorithm
        )
    }
}

impl KeyRequestMachine {
    async fn create_outgoing_key_request(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) {
        let key_info = RequestedKeyInfo {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            room_id: room_id.to_owned(),
            sender_key: sender_key.to_owned(),
            session_id: session_id.to_owned(),
        };

        let id: Option<String> = self.store.get_object(&key_info.encode()).await.unwrap();

        if id.is_some() {
            // We already sent out a request for this key, nothing to do.
            return;
        }

        info!("Creating new outgoing room key request {:#?}", key_info);

        let id = Uuid::new_v4();

        let content = RoomKeyRequestEventContent {
            action: Action::Request,
            request_id: id.to_string(),
            requesting_device_id: (&*self.device_id).clone(),
            body: Some(key_info),
        };

        let mut messages = BTreeMap::new();

        messages
            .entry((&*self.user_id).to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                DeviceIdOrAllDevices::AllDevices,
                to_raw_value(&content).unwrap(),
            );

        let request = OutgoingRequest {
            request_id: id,
            request: Arc::new(
                ToDeviceRequest {
                    event_type: EventType::RoomKeyRequest,
                    txn_id: id,
                    messages,
                }
                .into(),
            ),
        };

        let info = OugoingKeyInfo {
            request_id: id,
            info: content.body.unwrap(),
            sent_out: false,
        };

        self.save_outgoing_key_info(id, info).await;
        self.outgoing_to_device_requests.insert(id, request);
    }

    async fn save_outgoing_key_info(&self, id: Uuid, info: OugoingKeyInfo) {
        // We need to access the key info via the hash of an RequestedKeyInfo
        // and with the unique request id.
        // Once to mark the request as sent. And another time to check if the
        // received forwarded key matches to a request.
        let id_string = id.to_string();
        self.store.save_object(&id_string, &info).await.unwrap();
        self.store
            .save_object(&info.info.encode(), &id)
            .await
            .unwrap();
    }

    async fn get_key_info(&self, content: &ForwardedRoomKeyEventContent) -> Option<OugoingKeyInfo> {
        let id: Option<Uuid> = self.store.get_object(&content.encode()).await.unwrap();

        if let Some(id) = id {
            self.store.get_object(&id.to_string()).await.unwrap()
        } else {
            None
        }
    }

    async fn delete_key_info(&self, info: OugoingKeyInfo) {
        self.store
            .delete_object(&info.request_id.to_string())
            .await
            .unwrap();
        self.store.delete_object(&info.info.encode()).await.unwrap();
    }

    async fn mark_outgoing_request_as_sent(&self, id: Uuid) {
        self.outgoing_to_device_requests.remove(&id);
        let info: Option<OugoingKeyInfo> = self.store.get_object(&id.to_string()).await.unwrap();

        if let Some(mut info) = info {
            trace!("Marking outgoing key request as sent {:#?}", info);
            info.sent_out = true;
            self.save_outgoing_key_info(id, info).await;
        } else {
            error!("Trying to mark a room key request with the id {} as sent, but no key info was found", id);
        }
    }

    async fn receive_forwarded_room_key(
        &self,
        _sender_key: &str,
        _signing_key: &str,
        event: &mut ToDeviceEvent<ForwardedRoomKeyEventContent>,
    ) {
        let key_info = self.get_key_info(&event.content).await;

        if let Some(info) = key_info {
            // TODO create a new room key and store it if it's a better version
            // of the existing key or if we don't have it at all.
            self.outgoing_to_device_requests.remove(&info.request_id);
            self.delete_key_info(info).await;
        } else {
            info!(
                "Received a forwarded room key from {}, but no key info was found.",
                event.sender,
            );
        }
    }

    fn handle_incoming_key_request(&self, event: &ToDeviceEvent<RoomKeyRequestEventContent>) {
        match event.content.action {
            Action::Request => todo!(),
            Action::CancelRequest => todo!(),
        }
    }

    fn cancel_request(&self) {
        todo!()
    }
}
