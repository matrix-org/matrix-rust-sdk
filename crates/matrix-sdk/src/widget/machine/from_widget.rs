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
