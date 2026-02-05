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

//! DCGKA Manager for OlmMachine integration

use std::{collections::HashMap, sync::Arc};

use ruma::{OwnedRoomId, RoomId};
use tokio::sync::RwLock;

use super::DcgkaEngine;
use crate::store::Store;

/// Manager for DCGKA engines across multiple rooms
#[derive(Clone, Debug)]
pub struct DcgkaManager {
    /// Reference to the crypto store
    store: Store,
    /// Active DCGKA engines per room (room_id → engine)
    engines: Arc<RwLock<HashMap<OwnedRoomId, DcgkaEngine>>>,
    /// Rooms that have DCGKA enabled for transparent encryption
    enabled_rooms: Arc<RwLock<std::collections::HashSet<OwnedRoomId>>>,
}

impl DcgkaManager {
    /// Create a new DCGKA manager
    pub fn new(store: Store) -> Self {
        Self {
            store,
            engines: Arc::new(RwLock::new(HashMap::new())),
            enabled_rooms: Arc::new(RwLock::new(std::collections::HashSet::new())),
        }
    }

    /// Get or create a DCGKA engine for a specific room
    ///
    /// This will load the existing state from the store if available,
    /// or create a fresh engine if this is the first time we're using
    /// DCGKA in this room.
    ///
    /// Note: For PoC, we create a fresh engine each time. In production,
    /// we'd need to properly serialize/deserialize the entire engine state
    /// including the signing key.
    pub async fn get_or_create_engine(
        &self,
        room_id: &RoomId,
    ) -> Result<DcgkaEngine, crate::CryptoStoreError> {
        // Check if we already have an engine in memory
        {
            let engines = self.engines.read().await;
            if let Some(engine) = engines.get(room_id) {
                return Ok(engine.clone());
            }
        }

        // For PoC: Create a fresh engine with a new Ed25519 keypair
        // In production, we'd restore the keypair from the account
        let account = self.store.static_account();

        // Create a new Ed25519 keypair for this engine
        // TODO: In production, derive this from the device's identity key
        let signing_key = vodozemac::Ed25519Keypair::new();
        let identity_key = account.identity_keys.curve25519;

        // Create engine
        let engine = DcgkaEngine::new(
            room_id.to_owned(),
            self.store.user_id().to_owned(),
            self.store.device_id().to_owned(),
            signing_key,
            identity_key,
        );

        // Try to load and restore state from store
        if let Some(_state) = self.store.load_dcgka_state(room_id).await? {
            // TODO: Add a method to DcgkaEngine to restore state
            // For now, we just create a fresh engine
            tracing::warn!("Loaded DCGKA state for room but cannot restore to engine yet");
        }

        // Cache the engine
        let mut engines = self.engines.write().await;
        engines.insert(room_id.to_owned(), engine.clone());

        Ok(engine)
    }

    /// Save the state of a DCGKA engine for a room
    pub async fn save_engine_state(
        &self,
        room_id: &RoomId,
        engine: &DcgkaEngine,
    ) -> Result<(), crate::CryptoStoreError> {
        self.store.save_dcgka_state(room_id, engine.state().clone()).await?;

        // Update the in-memory cache
        let mut engines = self.engines.write().await;
        engines.insert(room_id.to_owned(), engine.clone());

        Ok(())
    }

    /// Enable DCGKA for a room (for transparent encryption)
    pub async fn enable_for_room(&self, room_id: &RoomId) {
        let mut enabled = self.enabled_rooms.write().await;
        enabled.insert(room_id.to_owned());
        tracing::info!("Enabled DCGKA for room {}", room_id);
    }

    /// Disable DCGKA for a room
    pub async fn disable_for_room(&self, room_id: &RoomId) {
        let mut enabled = self.enabled_rooms.write().await;
        enabled.remove(room_id);
        tracing::info!("Disabled DCGKA for room {}", room_id);
    }

    /// Check if DCGKA is enabled for a room
    pub async fn is_enabled_for_room(&self, room_id: &RoomId) -> bool {
        let enabled = self.enabled_rooms.read().await;
        enabled.contains(room_id)
    }
}
