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

//! Facilities to deduplicate similar queries running at the same time.
//!
//! See [`DeduplicatingHandler`].

use std::{collections::BTreeMap, sync::Arc};

use futures_core::Future;
use matrix_sdk_common::SendOutsideWasm;
use tokio::sync::Mutex;

use crate::{Error, Result};

/// State machine for the state of a query deduplicated by the
/// [`DeduplicatingHandler`].
enum QueryState {
    /// The query hasn't completed yet. This doesn't mean it hasn't *started*
    /// yet, but rather that it couldn't get to completion: some
    /// intermediate steps might have run.
    NotFinishedYet,
    /// The query has completed with an `Ok` result.
    Success,
    /// The query has completed with an `Err` result.
    Failure,
}

type DeduplicatedRequestMap<Key> = Mutex<BTreeMap<Key, Arc<Mutex<QueryState>>>>;

/// Handler that properly deduplicates function calls given a key uniquely
/// identifying the call kind, and will properly report error upwards in case
/// the concurrent call failed.
///
/// This is handy for deduplicating per-room requests, but can also be used in
/// other contexts.
pub(crate) struct DeduplicatingHandler<Key> {
    /// Map of outstanding function calls, grouped by key.
    inflight: DeduplicatedRequestMap<Key>,
}

impl<Key> Default for DeduplicatingHandler<Key> {
    fn default() -> Self {
        Self { inflight: Default::default() }
    }
}

impl<Key: Clone + Ord + std::hash::Hash> DeduplicatingHandler<Key> {
    /// Runs the given code if and only if there wasn't a similar query running
    /// for the same key.
    ///
    /// See also [`DeduplicatingHandler`] for more details.
    pub async fn run<'a, F: Future<Output = Result<()>> + SendOutsideWasm + 'a>(
        &self,
        key: Key,
        code: F,
    ) -> Result<()> {
        let mut map = self.inflight.lock().await;

        if let Some(request_mutex) = map.get(&key).cloned() {
            // If a request is already going on, await the release of the lock.
            drop(map);

            let mut request_guard = request_mutex.lock().await;

            return match *request_guard {
                QueryState::Success => {
                    // The query completed with a success: forward this success.
                    Ok(())
                }

                QueryState::Failure => {
                    // The query completed with an error, but we don't know what it is; report
                    // there was an error.
                    Err(Error::ConcurrentRequestFailed)
                }

                QueryState::NotFinishedYet => {
                    // If we could take a hold onto the mutex without it being in the success or
                    // failure state, then the query hasn't completed (e.g. it could have been
                    // cancelled). Repeat it.
                    //
                    // Note: there might be other waiters for the deduplicated result; they will
                    // still be waiting for the mutex above, since the mutex is obtained for at
                    // most one holder at the same time.
                    self.run_code(key, code, &mut request_guard).await
                }
            };
        }

        // Start at the `None` state to indicate we haven't completed the request yet.
        let request_mutex = Arc::new(Mutex::new(QueryState::NotFinishedYet));

        map.insert(key.clone(), request_mutex.clone());

        let mut request_guard = request_mutex.lock().await;
        drop(map);

        self.run_code(key, code, &mut request_guard).await
    }

    async fn run_code<'a, F: Future<Output = Result<()>> + SendOutsideWasm + 'a>(
        &self,
        key: Key,
        code: F,
        result: &mut QueryState,
    ) -> Result<()> {
        match code.await {
            Ok(()) => {
                // Mark the request as completed.
                *result = QueryState::Success;

                self.inflight.lock().await.remove(&key);

                Ok(())
            }

            Err(err) => {
                // Propagate the error state to other callers.
                *result = QueryState::Failure;

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
