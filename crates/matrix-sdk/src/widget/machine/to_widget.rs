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

use serde::Serialize;

/// A request that the driver can send to the widget.
///
/// In postmessage interface terms: an `"api": "toWidget"` message.
pub(crate) trait ToWidgetRequest: Serialize {
    const ACTION: &'static str;
}

/// Request the widget to send the list of capabilities that it wants to have.
#[derive(Serialize)]
pub(crate) struct RequestPermissions {}

impl ToWidgetRequest for RequestPermissions {
    const ACTION: &'static str = "capabilities";
}
