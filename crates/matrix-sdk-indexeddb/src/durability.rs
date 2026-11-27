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

//! Transaction durability helpers.
//!
//! Chromium defaults IndexedDB transactions to relaxed durability, under
//! which a committed record can outlive its large-value blob file after an
//! unclean shutdown, leaving the store permanently unreadable
//! (element-hq/element-web#27969). Readwrite transactions in the crypto,
//! state and media stores therefore request strict durability.

use indexed_db_futures::{
    database::TransactionBuilder,
    transaction::{TransactionDurability, TransactionMode, TransactionOptions},
};

/// Helpers to set a transaction's mode while requesting strict durability
/// for readwrite modes, and the default durability otherwise.
pub(crate) trait ReadwriteDurable<'a, S>: Sized {
    /// Sets the given mode, with strict durability if it is a readwrite mode.
    fn with_durable_mode(
        self,
        mode: TransactionMode,
    ) -> TransactionBuilder<'a, S, TransactionMode, TransactionOptions>;

    /// Shorthand for `.with_durable_mode(Readwrite)`.
    #[cfg(any(feature = "e2e-encryption", feature = "state-store"))]
    fn readwrite_durable(self) -> TransactionBuilder<'a, S, TransactionMode, TransactionOptions> {
        self.with_durable_mode(TransactionMode::Readwrite)
    }
}

impl<'a, S> ReadwriteDurable<'a, S> for TransactionBuilder<'a, S> {
    fn with_durable_mode(
        self,
        mode: TransactionMode,
    ) -> TransactionBuilder<'a, S, TransactionMode, TransactionOptions> {
        let durability = match mode {
            TransactionMode::Readwrite | TransactionMode::Readwriteflush => {
                TransactionDurability::Strict
            }
            _ => TransactionDurability::Default,
        };
        self.with_mode(mode).with_options(TransactionOptions::new().with_durability(durability))
    }
}
