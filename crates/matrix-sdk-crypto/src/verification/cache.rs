// Copyright 2021 The Matrix.org Foundation C.I.C.
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

use dashmap::DashMap;
use ruma::{DeviceId, OwnedTransactionId, OwnedUserId, TransactionId, UserId};
use tracing::{trace, warn};

use super::{event_enums::OutgoingContent, FlowId, Sas, Verification};
#[cfg(feature = "qrcode")]
use crate::QrVerification;
use crate::{OutgoingRequest, OutgoingVerificationRequest, RoomMessageRequest, ToDeviceRequest};

#[derive(Clone, Debug)]
pub struct VerificationCache {
    verification: Arc<DashMap<OwnedUserId, DashMap<String, Verification>>>,
    outgoing_requests: Arc<DashMap<OwnedTransactionId, OutgoingRequest>>,
    flow_ids_waiting_for_response: Arc<DashMap<OwnedTransactionId, (OwnedUserId, FlowId)>>,
}

#[derive(Debug)]
pub struct RequestInfo {
    pub flow_id: FlowId,
    pub request_id: OwnedTransactionId,
}

impl VerificationCache {
    pub fn new() -> Self {
        Self {
            verification: Default::default(),
            outgoing_requests: Default::default(),
            flow_ids_waiting_for_response: Default::default(),
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.verification.iter().all(|m| m.is_empty())
    }

    /// Add a new `Verification` object to the cache, this will cancel any
    /// duplicates we have going on, including the newly inserted one, with a
    /// given user.
    pub fn insert(&self, verification: impl Into<Verification>) {
        let verification = verification.into();

        let entry = self.verification.entry(verification.other_user().to_owned()).or_default();
        let user_verifications = entry.value();

        // Cancel all the old verifications as well as the new one we have for
        // this user if someone tries to have two verifications going on at
        // once.
        for old in user_verifications {
            let old_verification = old.value();

            if !old_verification.is_cancelled() {
                warn!(
                    user_id = verification.other_user().as_str(),
                    old_flow_id = old_verification.flow_id(),
                    new_flow_id = verification.flow_id(),
                    "Received a new verification whilst another one with \
                    the same user is ongoing. Cancelling both verifications"
                );

                if let Some(r) = old_verification.cancel() {
                    self.add_request(r.into())
                }

                if let Some(r) = verification.cancel() {
                    self.add_request(r.into())
                }
            }
        }

        // We still want to add the new verification, in case users want to
        // inspect the verification object a matching `m.key.verification.start`
        // produced.
        user_verifications.insert(verification.flow_id().to_owned(), verification);
    }

    pub fn insert_sas(&self, sas: Sas) {
        self.insert(sas);
    }

    pub fn replace_sas(&self, sas: Sas) {
        let verification: Verification = sas.into();
        self.replace(verification);
    }

    #[cfg(feature = "qrcode")]
    pub fn insert_qr(&self, qr: QrVerification) {
        self.insert(qr)
    }

    #[cfg(feature = "qrcode")]
    pub fn replace_qr(&self, qr: QrVerification) {
        let verification: Verification = qr.into();
        self.replace(verification);
    }

    #[cfg(feature = "qrcode")]
    pub fn get_qr(&self, sender: &UserId, flow_id: &str) -> Option<QrVerification> {
        self.get(sender, flow_id).and_then(|v| {
            if let Verification::QrV1(qr) = v {
                Some(qr)
            } else {
                None
            }
        })
    }

    pub fn replace(&self, verification: Verification) {
        self.verification
            .entry(verification.other_user().to_owned())
            .or_default()
            .insert(verification.flow_id().to_owned(), verification.clone());
    }

    pub fn get(&self, sender: &UserId, flow_id: &str) -> Option<Verification> {
        self.verification.get(sender)?.get(flow_id).map(|v| v.clone())
    }

    pub fn outgoing_requests(&self) -> Vec<OutgoingRequest> {
        self.outgoing_requests.iter().map(|r| (*r).clone()).collect()
    }

    pub fn garbage_collect(&self) -> Vec<OutgoingVerificationRequest> {
        for user_verification in self.verification.iter() {
            user_verification.retain(|_, s| !(s.is_done() || s.is_cancelled()));
        }

        self.verification.retain(|_, m| !m.is_empty());

        self.verification
            .iter()
            .flat_map(|v| {
                let requests: Vec<OutgoingVerificationRequest> = v
                    .value()
                    .iter()
                    .filter_map(|s| {
                        #[allow(irrefutable_let_patterns)]
                        if let Verification::SasV1(s) = s.value() {
                            s.cancel_if_timed_out()
                        } else {
                            None
                        }
                    })
                    .collect();

                requests
            })
            .collect()
    }

    pub fn get_sas(&self, user_id: &UserId, flow_id: &str) -> Option<Sas> {
        self.get(user_id, flow_id).and_then(|v| {
            #[allow(irrefutable_let_patterns)]
            if let Verification::SasV1(sas) = v {
                Some(sas)
            } else {
                None
            }
        })
    }

    pub fn add_request(&self, request: OutgoingRequest) {
        trace!("Adding an outgoing verification request {:?}", request);
        self.outgoing_requests.insert(request.request_id.clone(), request);
    }

    pub fn add_verification_request(&self, request: OutgoingVerificationRequest) {
        let request = OutgoingRequest {
            request_id: request.request_id().to_owned(),
            request: Arc::new(request.into()),
        };
        self.add_request(request);
    }

    pub fn queue_up_content(
        &self,
        recipient: &UserId,
        recipient_device: &DeviceId,
        content: OutgoingContent,
        request_info: Option<RequestInfo>,
    ) {
        let request_id = if let Some(request_info) = request_info {
            trace!(
                ?recipient,
                ?request_info,
                "Storing the request info, waiting for the request to be marked as sent"
            );

            self.flow_ids_waiting_for_response.insert(
                request_info.request_id.to_owned(),
                (recipient.to_owned(), request_info.flow_id),
            );
            request_info.request_id
        } else {
            TransactionId::new()
        };

        match content {
            OutgoingContent::ToDevice(c) => {
                let request =
                    ToDeviceRequest::with_id(recipient, recipient_device.to_owned(), c, request_id);
                let request_id = request.txn_id.clone();

                let request = OutgoingRequest {
                    request_id: request_id.clone(),
                    request: Arc::new(request.into()),
                };

                self.outgoing_requests.insert(request_id, request);
            }

            OutgoingContent::Room(r, c) => {
                let request = OutgoingRequest {
                    request: Arc::new(
                        RoomMessageRequest { room_id: r, txn_id: request_id.clone(), content: c }
                            .into(),
                    ),
                    request_id: request_id.clone(),
                };

                self.outgoing_requests.insert(request_id, request);
            }
        }
    }

    pub fn mark_request_as_sent(&self, request_id: &TransactionId) {
        if let Some(request_id) = self.outgoing_requests.remove(request_id) {
            trace!(?request_id, "Marking a verification HTTP request as sent");
        }

        if let Some((user_id, flow_id)) =
            self.flow_ids_waiting_for_response.get(request_id).as_deref()
        {
            if let Some(verification) = self.get(user_id, flow_id.as_str()) {
                match verification {
                    Verification::SasV1(s) => s.mark_request_as_sent(request_id),
                    #[cfg(feature = "qrcode")]
                    Verification::QrV1(_) => (),
                }
            }
        }
    }
}
