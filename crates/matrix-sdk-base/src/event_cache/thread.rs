// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! Threads types related to the Event Cache.

use serde::{Deserialize, Serialize};

use crate::read_receipts::ReadReceipts;

/// All the information about a thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadInfo {
    /// Read receipts for the current thread.
    pub read_receipts: ReadReceipts,
}

impl ThreadInfo {
    /// Create a new [`ThreadInfo`].
    pub fn new() -> Self {
        Self { read_receipts: ReadReceipts::default() }
    }
}

impl Default for ThreadInfo {
    fn default() -> Self {
        Self::new()
    }
}
