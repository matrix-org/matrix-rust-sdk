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
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::disallowed_methods)]
    tokio::time::sleep(duration).await;

    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(duration.as_millis() as u32).await;
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test_macros::async_test;

    use super::*;

    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    #[async_test]
    async fn test_sleep() {
        // Just test that it doesn't panic
        sleep(Duration::from_millis(1)).await;
    }
}
