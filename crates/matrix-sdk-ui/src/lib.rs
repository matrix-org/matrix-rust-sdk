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

use ruma::html::HtmlSanitizerMode;

mod events;

pub mod authentication;
pub mod encryption_sync_service;
pub mod notification_client;
pub mod room_list_service;
pub mod sync_service;
pub mod timeline;

pub use self::{room_list_service::RoomListService, timeline::Timeline};

/// The default sanitizer mode used when sanitizing HTML.
const DEFAULT_SANITIZER_MODE: HtmlSanitizerMode = HtmlSanitizerMode::Compat;

#[cfg(test)]
matrix_sdk_test::init_tracing_for_tests!();
