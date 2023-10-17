// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::marker::PhantomData;

use serde::{de::DeserializeOwned, Serialize};
use tracing::error;

use super::{ToWidgetRequestMeta, WidgetMachine};
use crate::widget::Permissions;

/// A handle to a pending `toWidget` request.
pub(crate) struct ToWidgetRequestHandle<'m, T> {
    request_meta: Option<&'m mut ToWidgetRequestMeta>,
    _phantom: PhantomData<fn() -> T>,
}

impl<'m, T> ToWidgetRequestHandle<'m, T>
where
    T: DeserializeOwned,
{
    pub(crate) fn new(request_meta: &'m mut ToWidgetRequestMeta) -> Self {
        Self { request_meta: Some(request_meta), _phantom: PhantomData }
    }

    pub(crate) fn null() -> Self {
        Self { request_meta: None, _phantom: PhantomData }
    }

    pub(crate) fn then(
        self,
        response_handler: impl FnOnce(T, &mut WidgetMachine) + Send + 'static,
    ) {
        if let Some(request_meta) = self.request_meta {
            request_meta.response_fn = Some(Box::new(move |raw_response_data, machine| {
                match serde_json::from_str(raw_response_data.get()) {
                    Ok(response_data) => response_handler(response_data, machine),
                    Err(e) => error!("Failed to deserialize toWidget response: {e}"),
                }
            }));
        }
    }
}

/// A request that the driver can send to the widget.
///
/// In postmessage interface terms: an `"api": "toWidget"` message.
pub(crate) trait ToWidgetRequest: Serialize {
    const ACTION: &'static str;
    type ResponseData: DeserializeOwned;
}

/// Request the widget to send the list of capabilities that it wants to have.
#[derive(Serialize)]
pub(crate) struct RequestPermissions {}

impl ToWidgetRequest for RequestPermissions {
    const ACTION: &'static str = "capabilities";
    type ResponseData = Permissions;
}

/// Notify the widget that the list of the granted capabilities has changed.
#[derive(Serialize)]
pub(crate) struct NotifyPermissionsChanged {
    pub(crate) requested: Permissions,
    pub(crate) approved: Permissions,
}

impl ToWidgetRequest for NotifyPermissionsChanged {
    const ACTION: &'static str = "notify_capabilities";
    type ResponseData = ();
}
