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

use std::ops::Deref;

use matrix_sdk_base::{Device as ReadOnlyDevice, DeviceWrap, UserDevicesWrap};
use matrix_sdk_common::{
    api::r0::to_device::send_event_to_device::Request as ToDeviceRequest, identifiers::DeviceId,
};

use crate::{error::Result, http_client::HttpClient, Sas};

#[derive(Clone, Debug)]
/// A device represents a E2EE capable client of an user.
pub struct Device {
    pub(crate) inner: DeviceWrap,
    pub(crate) http_client: HttpClient,
}

impl Deref for Device {
    type Target = ReadOnlyDevice;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Device {
    /// Start a interactive verification with this `Device`
    ///
    /// Returns a `Sas` object that represents the interactive verification flow.
    pub async fn start_verification(&self) -> Result<Sas> {
        let (sas, request) = self.inner.start_verification();
        let request = ToDeviceRequest {
            event_type: request.event_type,
            txn_id: &request.txn_id,
            messages: request.messages,
        };

        self.http_client.send(request).await?;

        Ok(Sas {
            inner: sas,
            http_client: self.http_client.clone(),
        })
    }
}

/// A read only view over all devices belonging to a user.
#[derive(Debug)]
pub struct UserDevices {
    pub(crate) inner: UserDevicesWrap,
    pub(crate) http_client: HttpClient,
}

impl UserDevices {
    /// Get the specific device with the given device id.
    pub fn get(&self, device_id: &DeviceId) -> Option<Device> {
        self.inner.get(device_id).map(|d| Device {
            inner: d,
            http_client: self.http_client.clone(),
        })
    }

    /// Iterator over all the device ids of the user devices.
    pub fn keys(&self) -> impl Iterator<Item = &DeviceId> {
        self.inner.keys()
    }

    /// Iterator over all the devices of the user devices.
    pub fn devices(&self) -> impl Iterator<Item = Device> + '_ {
        let client = self.http_client.clone();

        self.inner.devices().map(move |d| Device {
            inner: d,
            http_client: client.clone(),
        })
    }
}
