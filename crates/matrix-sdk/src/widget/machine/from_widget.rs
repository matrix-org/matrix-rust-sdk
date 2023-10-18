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

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", content = "data")]
pub(super) enum FromWidgetRequest {
    SupportedApiVersions {},
    ContentLoaded {},
}

#[derive(Serialize)]
pub(super) struct FromWidgetErrorResponse {
    error: FromWidgetError,
}

impl FromWidgetErrorResponse {
    pub(super) fn new(e: impl fmt::Display) -> Self {
        Self { error: FromWidgetError { message: e.to_string() } }
    }
}

#[derive(Serialize)]
struct FromWidgetError {
    message: String,
}

#[derive(Serialize)]
pub(super) struct SupportedApiVersionsResponse {
    versions: Vec<ApiVersion>,
}

impl SupportedApiVersionsResponse {
    pub(super) fn new() -> Self {
        Self {
            versions: vec![
                ApiVersion::V0_0_1,
                ApiVersion::V0_0_2,
                ApiVersion::MSC2762,
                ApiVersion::MSC2871,
                ApiVersion::MSC3819,
            ],
        }
    }
}

#[derive(Serialize)]
#[allow(dead_code)] // not all variants used right now
pub(super) enum ApiVersion {
    /// First stable version.
    #[serde(rename = "0.0.1")]
    V0_0_1,

    /// Second stable version.
    #[serde(rename = "0.0.2")]
    V0_0_2,

    /// Supports sending and receiving of events.
    #[serde(rename = "org.matrix.msc2762")]
    MSC2762,

    /// Supports sending of approved capabilities back to the widget.
    #[serde(rename = "org.matrix.msc2871")]
    MSC2871,

    /// Supports navigating to a URI.
    #[serde(rename = "org.matrix.msc2931")]
    MSC2931,

    /// Supports capabilities renegotiation.
    #[serde(rename = "org.matrix.msc2974")]
    MSC2974,

    /// Supports reading events in a room (deprecated).
    #[serde(rename = "org.matrix.msc2876")]
    MSC2876,

    /// Supports sending and receiving of to-device events.
    #[serde(rename = "org.matrix.msc3819")]
    MSC3819,

    /// Supports access to the TURN servers.
    #[serde(rename = "town.robin.msc3846")]
    MSC3846,
}
