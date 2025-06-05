// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use std::time::Duration;

/// Sleep for the specified duration.
///
/// This is a cross-platform sleep implementation that works on both wasm32 and
/// non-wasm32 targets.
pub async fn sleep(duration: Duration) {
    #[cfg(not(target_family = "wasm"))]
    tokio::time::sleep(duration).await;

    #[cfg(target_family = "wasm")]
    gloo_timers::future::TimeoutFuture::new(u32::try_from(duration.as_millis()).unwrap_or_else(
        |_| {
            tracing::error!("Sleep duration too long, sleeping for u32::MAX ms");
            u32::MAX
        },
    ))
    .await;
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test_macros::async_test;

    use super::*;

    #[cfg(target_family = "wasm")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    #[async_test]
    async fn test_sleep() {
        // Just test that it doesn't panic
        sleep(Duration::from_millis(1)).await;
    }
}
