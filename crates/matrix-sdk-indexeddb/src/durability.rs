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

use indexed_db_futures::{
    database::TransactionBuilder,
    transaction::{TransactionDurability, TransactionMode, TransactionOptions},
};

// Chromium defaults IndexedDB transactions to relaxed durability, under which
// a committed record can outlive its large-value blob file after an unclean
// shutdown, leaving the store permanently unreadable
// (element-hq/element-web#27969). Every readwrite transaction here therefore
// requests strict durability.

/// Options requesting strict durability for a readwrite mode, and the default
/// durability otherwise.
#[cfg(any(feature = "event-cache-store", feature = "media-store"))]
pub(crate) fn durability_for(mode: TransactionMode) -> TransactionOptions {
    let durability = match mode {
        TransactionMode::Readwrite | TransactionMode::Readwriteflush => {
            TransactionDurability::Strict
        }
        _ => TransactionDurability::Default,
    };
    TransactionOptions::new().with_durability(durability)
}

/// Shorthand for `.with_mode(Readwrite)` with strict durability.
#[cfg(any(feature = "e2e-encryption", feature = "state-store"))]
pub(crate) trait ReadwriteDurable<'a, S> {
    fn readwrite_durable(self) -> TransactionBuilder<'a, S, TransactionMode, TransactionOptions>;
}

#[cfg(any(feature = "e2e-encryption", feature = "state-store"))]
impl<'a, S> ReadwriteDurable<'a, S> for TransactionBuilder<'a, S> {
    fn readwrite_durable(self) -> TransactionBuilder<'a, S, TransactionMode, TransactionOptions> {
        self.with_mode(TransactionMode::Readwrite)
            .with_options(TransactionOptions::new().with_durability(TransactionDurability::Strict))
    }
}
