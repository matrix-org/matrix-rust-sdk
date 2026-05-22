// Copyright 2026 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0

//! Live MSC3814 round trip against a real homeserver.
//!
//! Validates that the [`DehydratedDevices`] manager can:
//!
//!  1. Probe support.
//!  2. Create and upload a dehydrated device with a locally generated pickle
//!     key.
//!  3. Rehydrate the very same device and observe that the server-side device
//!     is gone afterwards.
//!  4. Drive the same lifecycle from `start` after bootstrapping Recovery,
//!     proving that the Secret Storage pickle-key round trip works against a
//!     real account-data backend.
//!
//! The test points at `HOMESERVER_URL` and registers a fresh user, so it
//! works against Synapse and Tuwunel without code changes.

use std::time::Duration;

use anyhow::Result;
use assert_matches2::assert_let;
use futures::StreamExt;
use matrix_sdk::{
    encryption::{
        EncryptionSettings,
        dehydrated_devices::{DehydratedDeviceEvent, StartDehydrationOpts},
    },
    timeout::timeout,
};
use matrix_sdk_base::crypto::store::types::DehydratedDeviceKey;
use tracing::{info, warn};

use crate::helpers::TestClientBuilder;

/// Direct `create` → `rehydrate` round trip without involving Secret Storage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_dehydrated_device_direct_round_trip() -> Result<()> {
    // Cross-signing must be bootstrapped: a dehydrated device upload signs
    // the device keys with the local self-signing key.
    let encryption_settings =
        EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };

    let alice = TestClientBuilder::new("alice_dehydrated_direct")
        .use_sqlite()
        .encryption_settings(encryption_settings)
        .build()
        .await?;
    alice.encryption().wait_for_e2ee_initialization_tasks().await;

    let dehydrated = alice.encryption().dehydrated_devices();
    if !dehydrated.is_supported().await? {
        warn!("Homeserver does not advertise MSC3814; skipping the dehydrated-device round trip");
        return Ok(());
    }

    let mut events = Box::pin(dehydrated.events());

    let pickle_key = DehydratedDeviceKey::new();
    let device_id = dehydrated.create(Some("Direct round trip"), &pickle_key).await?;
    info!(?device_id, "Alice uploaded a dehydrated device");

    let mut saw_created = false;
    let mut saw_uploaded = false;
    let mut saw_rh_started = false;
    let mut saw_rh_completed = false;
    let mut saw_deleted = false;

    // Rehydrate first to emit the matching events; observe both lifecycles
    // off the same stream in one drain.
    assert!(dehydrated.rehydrate(&pickle_key).await?);

    timeout(
        async {
            while !(saw_created
                && saw_uploaded
                && saw_rh_started
                && saw_rh_completed
                && saw_deleted)
            {
                let event = events.next().await.expect("event stream is open")?;
                match event {
                    DehydratedDeviceEvent::Created { device_id: id } => {
                        assert_eq!(id, device_id);
                        saw_created = true;
                    }
                    DehydratedDeviceEvent::Uploaded { device_id: id } => {
                        assert_eq!(id, device_id);
                        saw_uploaded = true;
                    }
                    DehydratedDeviceEvent::RehydrationStarted { device_id: id } => {
                        assert_eq!(id, device_id);
                        saw_rh_started = true;
                    }
                    DehydratedDeviceEvent::RehydrationCompleted { device_id: id, .. } => {
                        assert_eq!(id, device_id);
                        saw_rh_completed = true;
                    }
                    DehydratedDeviceEvent::Deleted => saw_deleted = true,
                    _ => {}
                }
            }
            Ok::<_, anyhow::Error>(())
        },
        Duration::from_secs(15),
    )
    .await??;

    // The server-side device is gone; rehydrate now returns false.
    assert_let!(Ok(false) = dehydrated.rehydrate(&pickle_key).await);

    Ok(())
}

/// Full `start` lifecycle with Recovery wired up: the pickle key is
/// resolved out of Secret Storage rather than supplied by the caller.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_dehydrated_device_start_via_recovery() -> Result<()> {
    let encryption_settings = EncryptionSettings {
        auto_enable_cross_signing: true,
        auto_enable_backups: true,
        ..Default::default()
    };

    let alice = TestClientBuilder::new("alice_dehydrated_start")
        .use_sqlite()
        .encryption_settings(encryption_settings)
        .build()
        .await?;

    alice.encryption().wait_for_e2ee_initialization_tasks().await;

    let recovery = alice.encryption().recovery();
    let recovery_key = recovery.enable().await?;
    info!("Alice has enabled recovery; the pickle key lives in Secret Storage");

    let dehydrated = alice.encryption().dehydrated_devices();
    if !dehydrated.is_supported().await? {
        warn!("Homeserver does not advertise MSC3814; skipping the start-via-recovery flow");
        return Ok(());
    }

    let secret_store = alice.encryption().secret_storage().open_secret_store(&recovery_key).await?;

    let mut events = Box::pin(dehydrated.events());

    dehydrated.start(&secret_store, StartDehydrationOpts::default()).await?;
    let first_id = timeout(
        async {
            loop {
                let event = events.next().await.expect("event stream is open")?;
                if let DehydratedDeviceEvent::Uploaded { device_id } = event {
                    return Ok::<_, anyhow::Error>(device_id);
                }
            }
        },
        Duration::from_secs(15),
    )
    .await??;
    info!(?first_id, "Alice's first start uploaded a dehydrated device");

    dehydrated.start(&secret_store, StartDehydrationOpts::default()).await?;
    let second_id = timeout(
        async {
            loop {
                let event = events.next().await.expect("event stream is open")?;
                if let DehydratedDeviceEvent::Uploaded { device_id } = event {
                    return Ok::<_, anyhow::Error>(device_id);
                }
            }
        },
        Duration::from_secs(15),
    )
    .await??;
    assert_ne!(second_id, first_id, "the rotation must produce a different device id");

    dehydrated.stop();
    dehydrated.delete().await?;

    Ok(())
}
