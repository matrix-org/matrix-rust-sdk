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
use matrix_sdk_common::{
    identifiers::{DeviceId, UserId},
    uuid::Uuid,
};

use super::{event_enums::OutgoingContent, sas::content_to_request, Sas, Verification};
use crate::{OutgoingRequest, RoomMessageRequest};

#[derive(Clone, Debug)]
pub struct VerificationCache {
    verification: Arc<DashMap<String, Verification>>,
    outgoing_requests: Arc<DashMap<Uuid, OutgoingRequest>>,
}

impl VerificationCache {
    pub fn new() -> Self {
        Self { verification: DashMap::new().into(), outgoing_requests: DashMap::new().into() }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.verification.is_empty()
    }

    pub fn insert_sas(&self, sas: Sas) {
        self.verification.insert(sas.flow_id().as_str().to_string(), sas.into());
    }

    pub fn outgoing_requests(&self) -> Vec<OutgoingRequest> {
        self.outgoing_requests.iter().map(|r| (*r).clone()).collect()
    }

    pub fn garbage_collect(&self) -> Vec<OutgoingRequest> {
        self.verification.retain(|_, s| !(s.is_done() || s.is_cancelled()));

        self.verification
            .iter()
            .filter_map(|s| {
                #[allow(irrefutable_let_patterns)]
                if let Verification::SasV1(s) = s.value() {
                    s.cancel_if_timed_out().map(|r| OutgoingRequest {
                        request_id: r.request_id(),
                        request: Arc::new(r.into()),
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn get_sas(&self, transaction_id: &str) -> Option<Sas> {
        self.verification.get(transaction_id).and_then(|v| {
            #[allow(irrefutable_let_patterns)]
            if let Verification::SasV1(sas) = v.value() {
                Some(sas.clone())
            } else {
                None
            }
        })
    }

    pub fn add_request(&self, request: OutgoingRequest) {
        self.outgoing_requests.insert(request.request_id, request);
    }

    pub fn queue_up_content(
        &self,
        recipient: &UserId,
        recipient_device: &DeviceId,
        content: OutgoingContent,
    ) {
        match content {
            OutgoingContent::ToDevice(c) => {
                let request = content_to_request(recipient, recipient_device.to_owned(), c);
                let request_id = request.txn_id;

                let request = OutgoingRequest { request_id, request: Arc::new(request.into()) };

                self.outgoing_requests.insert(request_id, request);
            }

            OutgoingContent::Room(r, c) => {
                let request_id = Uuid::new_v4();

                let request = OutgoingRequest {
                    request: Arc::new(
                        RoomMessageRequest { room_id: r, txn_id: request_id, content: c }.into(),
                    ),
                    request_id,
                };

                self.outgoing_requests.insert(request_id, request);
            }
        }
    }

    pub fn mark_request_as_sent(&self, uuid: &Uuid) {
        self.outgoing_requests.remove(uuid);
    }
}
