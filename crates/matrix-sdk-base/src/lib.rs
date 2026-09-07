// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
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

#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(target_family = "wasm", allow(clippy::arc_with_non_send_sync))]
#![warn(missing_docs, missing_debug_implementations)]

use std::{
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

pub use matrix_sdk_common::*;
use ruma::{OwnedDeviceId, OwnedUserId};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

pub use crate::error::{Error, Result};

mod client;
pub use client::RequestedRequiredStates;
pub mod debug;
pub mod deserialized_responses;
mod error;
pub mod event_cache;
pub mod latest_event;
pub mod media;
pub mod notification_settings;
pub mod read_receipts;
mod response_processors;
mod room;

pub mod sliding_sync;

pub mod store;
pub mod sync;
#[cfg(any(test, feature = "testing"))]
mod test_utils;
mod utils;

pub use client::DmRoomDefinition;

#[cfg(feature = "experimental-element-recent-emojis")]
pub mod recent_emojis;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

pub use client::{BaseClient, ThreadingSupport};
#[cfg(any(test, feature = "testing"))]
pub use http;
#[cfg(feature = "e2e-encryption")]
pub use matrix_sdk_crypto as crypto;
pub use room::{
    CallIntentConsensus, EncryptionState, PredecessorRoom, Room, RoomCreateWithCreatorEventContent,
    RoomDisplayName, RoomHero, RoomHeroWithProfile, RoomInfo, RoomInfoNotableUpdate,
    RoomInfoNotableUpdateReasons, RoomMember, RoomMembersUpdate, RoomMemberships, RoomRecencyStamp,
    RoomState, RoomStateFilter, SuccessorRoom, apply_redaction,
};
pub use store::{
    ComposerDraft, ComposerDraftType, DraftAttachment, DraftAttachmentContent, DraftThumbnail,
    QueueWedgeError, StateChanges, StateStore, StateStoreDataKey, StateStoreDataValue, StoreError,
    ThreadSubscriptionCatchupToken,
};
pub use utils::{MinimalRoomMemberEvent, MinimalStateEvent, RawStateEventWithKeys};

#[cfg(test)]
matrix_sdk_test_utils::init_tracing_for_tests!();

/// The Matrix user session info.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// The ID of the session's user.
    pub user_id: OwnedUserId,
    /// The ID of the client device.
    pub device_id: OwnedDeviceId,
}

/// A future that can be cancelled cooperatively, created with
/// [`CancellableIntoFutureExt::cancellable`].
///
/// Resolves to `Some` with the output of the underlying future if it completes
/// first, or to `None` if it is cancelled first, either with [`Self::cancel`]
/// or through the [`CancellationToken`] returned by
/// [`Self::cancellation_token`]. The latter can be handed to another task to
/// cancel the future while it is being awaited.
pub struct Cancellable<'a, T> {
    token: CancellationToken,
    future: BoxFuture<'a, Option<T>>,
}

impl<T> Cancellable<'_, T> {
    /// Get a [`CancellationToken`] to cancel this future from elsewhere.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Cancel this future, making it resolve to `None`.
    pub fn cancel(&self) {
        self.token.cancel();
    }
}

impl<T> Future for Cancellable<'_, T> {
    type Output = Option<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.future.as_mut().poll(cx)
    }
}

impl<T> fmt::Debug for Cancellable<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cancellable").field("token", &self.token).finish_non_exhaustive()
    }
}

/// Extension trait for wrapping an [`IntoFuture`] with cooperative
/// cancellation.
pub trait CancellableIntoFutureExt: IntoFuture + Sized {
    /// Wrap this future in a [`Cancellable`], which resolves to `Some` with the
    /// output of this future if it completes first, or to `None` if it is
    /// cancelled first.
    fn cancellable<'a>(self) -> Cancellable<'a, Self::Output>
    where
        Self::IntoFuture: SendOutsideWasm + 'a,
    {
        let token = CancellationToken::new();
        let future = Box::pin(token.clone().run_until_cancelled_owned(self.into_future()));

        Cancellable { token, future }
    }
}

impl<T: IntoFuture> CancellableIntoFutureExt for T {}

#[cfg(test)]
mod tests {
    use std::{
        future::pending,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use matrix_sdk_common::executor::spawn;
    use matrix_sdk_test::async_test;

    use super::CancellableIntoFutureExt;

    #[async_test]
    async fn test_cancellable_resolves_to_some_when_not_cancelled() {
        assert_eq!(async { 42 }.cancellable().await, Some(42));
    }

    #[async_test]
    async fn test_cancellable_resolves_to_none_when_cancelled_before_being_polled() {
        let polled = Arc::new(AtomicBool::new(false));
        let future = {
            let polled = polled.clone();
            async move {
                polled.store(true, Ordering::SeqCst);
                42
            }
        };

        let cancellable = future.cancellable();
        cancellable.cancel();

        assert_eq!(cancellable.await, None);
        // The wrapped future isn't even polled once when the cancellation
        // happened before it was first awaited.
        assert!(!polled.load(Ordering::SeqCst));
    }

    #[async_test]
    async fn test_cancellable_resolves_to_none_when_cancelled_from_another_task() {
        // Use a future that never completes so that only the cancellation can
        // make the `Cancellable` resolve.
        let cancellable = pending::<()>().cancellable();
        let token = cancellable.cancellation_token();

        spawn(async move { token.cancel() });

        assert_eq!(cancellable.await, None);
    }
}
