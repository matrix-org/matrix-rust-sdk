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
    /// The query hasn't completed. This doesn't mean it hasn't *started* yet,
    /// but rather that it couldn't get to completion: some intermediate
    /// steps might have run.
    Cancelled,
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
    /// Note: the `code` may be run multiple times, if the first query to run it
    /// has been aborted by the caller (i.e. the future has been dropped).
    /// As a consequence, it's important that the `code` future be
    /// idempotent.
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

                QueryState::Cancelled => {
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

        // Let's assume the cancelled state, if we succeed or fail we'll modify the
        // result.
        let request_mutex = Arc::new(Mutex::new(QueryState::Cancelled));

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
#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::sync::Arc;

    use matrix_sdk_test::async_test;
    use tokio::{join, spawn, sync::Mutex, task::yield_now};

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

    #[async_test]
    async fn test_cancelling_deduplicated_query() -> anyhow::Result<()> {
        // A mutex used to prevent progress in the `inner` function.
        let allow_progress = Arc::new(Mutex::new(()));

        // Number of calls up to the `allow_progress` lock taking.
        let num_before = Arc::new(Mutex::new(0));
        // Number of calls after the `allow_progress` lock taking.
        let num_after = Arc::new(Mutex::new(0));

        let inner = || {
            let num_before = num_before.clone();
            let num_after = num_after.clone();
            let allow_progress = allow_progress.clone();

            async move {
                *num_before.lock().await += 1;
                let _ = allow_progress.lock().await;
                *num_after.lock().await += 1;
                Ok(())
            }
        };

        let handler = Arc::new(DeduplicatingHandler::default());

        // First, take the lock so that the `inner` can't complete.
        let progress_guard = allow_progress.lock().await;

        // Then, spawn deduplicated tasks.
        let first = spawn({
            let handler = handler.clone();
            let query = inner();
            async move { handler.run(0, query).await }
        });

        let second = spawn({
            let handler = handler.clone();
            let query = inner();
            async move { handler.run(0, query).await }
        });

        // At this point, only the "before" count has been incremented, and only once
        // (per the deduplication contract).
        yield_now().await;

        assert_eq!(*num_before.lock().await, 1);
        assert_eq!(*num_after.lock().await, 0);

        // Cancel the first task.
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        // The second task restarts the whole query from the beginning.
        yield_now().await;

        assert_eq!(*num_before.lock().await, 2);
        assert_eq!(*num_after.lock().await, 0);

        // Release the progress lock; the second query can now finish.
        drop(progress_guard);

        assert!(second.await.unwrap().is_ok());

        // We should've reached completion once.
        assert_eq!(*num_before.lock().await, 2);
        assert_eq!(*num_after.lock().await, 1);

        Ok(())
    }
}
