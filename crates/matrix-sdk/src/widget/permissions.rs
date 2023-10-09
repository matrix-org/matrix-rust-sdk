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

//! Types and traits related to the permissions that a widget can request from a
//! client.

use async_trait::async_trait;

use super::EventFilter;

/// Must be implemented by a component that provides functionality of deciding
/// whether a widget is allowed to use certain capabilities (typically by
/// providing a prompt to the user).
#[async_trait]
pub trait PermissionsProvider: Send + Sync + 'static {
    /// Receives a request for given permissions and returns the actual
    /// permissions that the clients grants to a given widget (usually by
    /// prompting the user).
    async fn acquire_permissions(&self, permissions: Permissions) -> Permissions;
}

/// Permissions that a widget can request from a client.
#[derive(Clone, Debug)]
pub struct Permissions {
    /// Types of the messages that a widget wants to be able to fetch.
    pub read: Vec<EventFilter>,
    /// Types of the messages that a widget wants to be able to send.
    pub send: Vec<EventFilter>,
    /// If this permission is requested by the widget, it can not operate
    /// separately from the matrix client.
    ///
    /// This means clients should not offer to open the widget in a separate
    /// browser/tab/webview that is not connected to the postmessage widget-api.
    pub requires_client: bool,
}
