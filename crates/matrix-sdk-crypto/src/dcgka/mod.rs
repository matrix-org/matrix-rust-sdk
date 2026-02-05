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

//! Decentralized Continuous Group Key Agreement (DCGKA) proof-of-concept
//!
//! This module implements a PoC DCGKA backend for matrix-rust-sdk that demonstrates
//! correct convergent behavior under concurrent operations and out-of-order message delivery.
//!
//! # Overview
//!
//! DCGKA provides group key agreement without requiring:
//! - Global ordering of operations
//! - Leader election
//! - Server-side awareness
//!
//! The implementation uses a dependency-based state machine where updates can be:
//! - **Accepted**: All dependencies satisfied, validation passed
//! - **Pending**: Awaiting dependency resolution
//! - **Rejected**: Validation failed (terminal state)
//!
//! # Example
//!
//! ```ignore
//! use matrix_sdk_crypto::dcgka::{DcgkaEngine, UpdateType};
//!
//! // Create engine for a room
//! let engine = DcgkaEngine::new(room_id, device_keys);
//!
//! // Apply an update (e.g., Add member)
//! let update = create_add_update(member_id, member_public_key);
//! engine.apply_update(update)?;
//!
//! // Derive current group key
//! let group_key = engine.derive_key()?;
//!
//! // Encrypt with group key
//! let ciphertext = engine.encrypt(plaintext)?;
//! ```
//!
//! # Key Guarantees
//!
//! - **Convergence**: Devices receiving same updates in different orders converge to identical keys
//! - **Safety**: Invalid updates move to Rejected state and never contribute to keys
//! - **Concurrency**: Multiple devices can issue updates simultaneously without coordination
//!
//! # Non-Goals (PoC Scope)
//!
//! - MLS protocol compatibility
//! - Byzantine fault tolerance
//! - Production features (backups, UX, etc.)

mod crypto;
mod engine;
mod manager;
mod state;
mod update;

#[cfg(test)]
mod tests;

pub use engine::{DcgkaEngine, DcgkaError, UpdateStatus};
pub use manager::DcgkaManager;
pub use state::DcgkaState;
pub use update::{DcgkaUpdate, UpdatePayload, UpdateType};
