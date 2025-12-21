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

use std::{collections::BTreeMap, iter};

use ruma::{
    OwnedDeviceId, OwnedTransactionId, OwnedUserId, TransactionId, UserId,
    events::{AnyToDeviceEventContent, ToDeviceEventContent, ToDeviceEventType},
    serde::Raw,
    to_device::DeviceIdOrAllDevices,
};
use serde::{Deserialize, Serialize};

/// Customized version of
/// `ruma_client_api::to_device::send_event_to_device::v3::Request`
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToDeviceRequest {
    /// Type of event being sent to each device.
    pub event_type: ToDeviceEventType,

    /// A request identifier unique to the access token used to send the
    /// request.
    pub txn_id: OwnedTransactionId,

    /// A map of users to devices to a content for a message event to be
    /// sent to the user's device. Individual message events can be sent
    /// to devices, but all events must be of the same type.
    /// The content's type for this field will be updated in a future
    /// release, until then you can create a value using
    /// `serde_json::value::to_raw_value`.
    pub messages:
        BTreeMap<OwnedUserId, BTreeMap<DeviceIdOrAllDevices, Raw<AnyToDeviceEventContent>>>,
}

impl ToDeviceRequest {
    /// Create a new owned to-device request
    ///
    /// # Arguments
    ///
    /// * `recipient` - The ID of the user that should receive this to-device
    ///   event.
    ///
    /// * `recipient_device` - The device that should receive this to-device
    ///   event, or all devices.
    ///
    /// * `event_type` - The type of the event content that is getting sent out.
    ///
    /// * `content` - The content of the to-device event.
    pub fn new(
        recipient: &UserId,
        recipient_device: impl Into<DeviceIdOrAllDevices>,
        event_type: &str,
        content: Raw<AnyToDeviceEventContent>,
    ) -> Self {
        let event_type = ToDeviceEventType::from(event_type);
        let user_messages = iter::once((recipient_device.into(), content)).collect();
        let messages = iter::once((recipient.to_owned(), user_messages)).collect();

        ToDeviceRequest { event_type, txn_id: TransactionId::new(), messages }
    }

    pub(crate) fn for_recipients(
        recipient: &UserId,
        recipient_devices: Vec<OwnedDeviceId>,
        content: &AnyToDeviceEventContent,
        txn_id: OwnedTransactionId,
    ) -> Self {
        let event_type = content.event_type();
        let raw_content = Raw::new(content).expect("Failed to serialize to-device event");

        if recipient_devices.is_empty() {
            Self::new(
                recipient,
                DeviceIdOrAllDevices::AllDevices,
                &event_type.to_string(),
                raw_content,
            )
        } else {
            let device_messages = recipient_devices
                .into_iter()
                .map(|d| (DeviceIdOrAllDevices::DeviceId(d), raw_content.clone()))
                .collect();

            let messages = iter::once((recipient.to_owned(), device_messages)).collect();

            ToDeviceRequest { event_type, txn_id, messages }
        }
    }

    pub(crate) fn with_id_raw(
        recipient: &UserId,
        recipient_device: impl Into<DeviceIdOrAllDevices>,
        content: Raw<AnyToDeviceEventContent>,
        event_type: ToDeviceEventType,
        txn_id: OwnedTransactionId,
    ) -> Self {
        let user_messages = iter::once((recipient_device.into(), content)).collect();
        let messages = iter::once((recipient.to_owned(), user_messages)).collect();

        ToDeviceRequest { event_type, txn_id, messages }
    }

    pub(crate) fn with_id(
        recipient: &UserId,
        recipient_device: impl Into<DeviceIdOrAllDevices>,
        content: &AnyToDeviceEventContent,
        txn_id: OwnedTransactionId,
    ) -> Self {
        let event_type = content.event_type();
        let raw_content = Raw::new(content).expect("Failed to serialize to-device event");

        let user_messages = iter::once((recipient_device.into(), raw_content)).collect();
        let messages = iter::once((recipient.to_owned(), user_messages)).collect();

        ToDeviceRequest { event_type, txn_id, messages }
    }

    /// Get the number of unique messages this request contains.
    ///
    /// *Note*: A single message may be sent to multiple devices, so this may or
    /// may not be the number of devices that will receive the messages as well.
    pub fn message_count(&self) -> usize {
        self.messages.values().map(|d| d.len()).sum()
    }
}
