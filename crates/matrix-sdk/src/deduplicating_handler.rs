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

use std::{collections::BTreeMap, sync::Arc};

use futures_core::Future;
use matrix_sdk_common::SendOutsideWasm;
use tokio::sync::Mutex;

use crate::{Error, Result};

type DeduplicatedRequestMap<Key> = Mutex<BTreeMap<Key, Arc<Mutex<Result<(), ()>>>>>;

/// Handler that properly deduplicates function calls given a key uniquely
/// identifying the call kind, and will properly report error upwards in case
/// the concurrent call failed.
///
/// This is handy for deduplicating per-room requests, but can also be used in
/// other contexts.
pub(crate) struct DeduplicatingHandler<Key> {
    inflight: DeduplicatedRequestMap<Key>,
}

impl<Key> Default for DeduplicatingHandler<Key> {
    fn default() -> Self {
        Self { inflight: Default::default() }
    }
}

impl<Key: Clone + Ord + std::hash::Hash> DeduplicatingHandler<Key> {
    pub async fn run<'a, F: Future<Output = Result<()>> + SendOutsideWasm + 'a>(
        &self,
        key: Key,
        code: F,
    ) -> Result<()> {
        let mut map = self.inflight.lock().await;

        if let Some(mutex) = map.get(&key).cloned() {
            // If a request is already going on, await the release of the lock.
            drop(map);

            return mutex.lock().await.map_err(|()| Error::ConcurrentRequestFailed);
        }

        // Assume a successful request; we'll modify the result in case of failures
        // later.
        let request_mutex = Arc::new(Mutex::new(Ok(())));

        map.insert(key.clone(), request_mutex.clone());

        let mut request_guard = request_mutex.lock().await;
        drop(map);

        match code.await {
            Ok(()) => {
                self.inflight.lock().await.remove(&key);
                Ok(())
            }

            Err(err) => {
                // Propagate the error state to other callers.
                *request_guard = Err(());

                // Remove the request from the in-flights set.
                self.inflight.lock().await.remove(&key);

                // Bubble up the error.
                Err(err)
            }
        }
    }
}

// Sorry wasm32, you don't have tokio::join :(
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::sync::Arc;

    use matrix_sdk_test::async_test;
    use tokio::{join, sync::Mutex, task::yield_now};

    use crate::deduplicating_handler::DeduplicatingHandler;

    #[async_test]
    async fn test_deduplicating_handler_same_key() -> anyhow::Result<()> {
        let num_calls = Arc::new(Mutex::new(0));

        let inner = || {
            let num_calls_cloned = num_calls.clone();
            async move {
                yield_now().await;
                *num_calls_cloned.lock().await += 1;
                yield_now().await;
                Ok(())
            }
        };

        let handler = DeduplicatingHandler::default();

        let (first, second) = join!(handler.run(0, inner()), handler.run(0, inner()));

        assert!(first.is_ok());
        assert!(second.is_ok());
        assert_eq!(*num_calls.lock().await, 1);

        Ok(())
    }

    #[async_test]
    async fn test_deduplicating_handler_different_keys() -> anyhow::Result<()> {
        let num_calls = Arc::new(Mutex::new(0));

        let inner = || {
            let num_calls_cloned = num_calls.clone();
            async move {
                yield_now().await;
                *num_calls_cloned.lock().await += 1;
                yield_now().await;
                Ok(())
            }
        };

        let handler = DeduplicatingHandler::default();

        let (first, second) = join!(handler.run(0, inner()), handler.run(1, inner()));

        assert!(first.is_ok());
        assert!(second.is_ok());
        assert_eq!(*num_calls.lock().await, 2);

        Ok(())
    }

    #[async_test]
    async fn test_deduplicating_handler_failure() -> anyhow::Result<()> {
        let num_calls = Arc::new(Mutex::new(0));

        let inner = || {
            let num_calls_cloned = num_calls.clone();
            async move {
                yield_now().await;
                *num_calls_cloned.lock().await += 1;
                yield_now().await;
                Err(crate::Error::AuthenticationRequired)
            }
        };

        let handler = DeduplicatingHandler::default();

        let (first, second) = join!(handler.run(0, inner()), handler.run(0, inner()));

        assert!(first.is_err());
        assert!(second.is_err());
        assert_eq!(*num_calls.lock().await, 1);

        // Then we can still do subsequent requests that may succeed (or fail), for the
        // same key.
        let inner = || {
            let num_calls_cloned = num_calls.clone();
            async move {
                *num_calls_cloned.lock().await += 1;
                Ok(())
            }
        };

        *num_calls.lock().await = 0;
        handler.run(0, inner()).await?;
        assert_eq!(*num_calls.lock().await, 1);

        Ok(())
    }
}
