// Copyright 2022 The Matrix.org Foundation C.I.C.
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

use std::{error::Error, fmt, time::Duration};

use futures_core::Future;
#[cfg(target_family = "wasm")]
use futures_util::future::{Either, select};
#[cfg(target_family = "wasm")]
use gloo_timers::future::TimeoutFuture;
#[cfg(not(target_family = "wasm"))]
use tokio::time::timeout as tokio_timeout;

/// Error type notifying that a timeout has elapsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElapsedError();

impl fmt::Display for ElapsedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "time waiting for future has elapsed!")
    }
}

impl Error for ElapsedError {}

/// Wait for `future` to be completed. `future` needs to return
/// a `Result`.
///
/// If the given timeout has elapsed the method will stop waiting and return
/// an error.
pub async fn timeout<F, T>(future: F, duration: Duration) -> Result<T, ElapsedError>
where
    F: Future<Output = T>,
{
    #[cfg(not(target_family = "wasm"))]
    return tokio_timeout(duration, future).await.map_err(|_| ElapsedError());

    #[cfg(target_family = "wasm")]
    {
        let timeout_future =
            TimeoutFuture::new(u32::try_from(duration.as_millis()).expect("Overlong duration"));

        match select(std::pin::pin!(future), timeout_future).await {
            Either::Left((res, _)) => Ok(res),
            Either::Right((_, _)) => Err(ElapsedError()),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{future, time::Duration};

    use matrix_sdk_test_macros::async_test;

    use super::timeout;

    #[cfg(target_family = "wasm")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    #[async_test]
    async fn test_without_timeout() {
        timeout(future::ready(()), Duration::from_millis(100))
            .await
            .expect("future should have completed without ElapsedError");
    }

    #[async_test]
    async fn test_with_timeout() {
        timeout(future::pending::<()>(), Duration::from_millis(100))
            .await
            .expect_err("future should return an ElapsedError");
    }
}
