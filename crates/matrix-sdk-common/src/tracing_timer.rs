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

use std::time::Instant;

use tracing::{callsite::DefaultCallsite, Callsite as _};

/// A named RAII that will show on Drop how long its covered section took to
/// execute.
pub struct TracingTimer {
    id: String,
    callsite: &'static DefaultCallsite,
    start: Instant,
    level: tracing::Level,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for TracingTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TracingTimer").field("id", &self.id).field("start", &self.start).finish()
    }
}

impl Drop for TracingTimer {
    fn drop(&mut self) {
        let message = format!("{} finished in {}ms", self.id, self.start.elapsed().as_millis());

        let enabled = tracing::level_enabled!(self.level) && {
            let interest = self.callsite.interest();
            !interest.is_never()
                && tracing::__macro_support::__is_enabled(self.callsite.metadata(), interest)
        };

        if !enabled {
            return;
        }

        let metadata = self.callsite.metadata();
        let fields = metadata.fields();
        let message_field = fields.field("message").unwrap();
        #[allow(trivial_casts)] // The compiler is lying, it can't infer this cast
        let values = [(&message_field, Some(&message as &dyn tracing::Value))];

        // This function is hidden from docs, but we have to use it
        // because there is no other way of obtaining a `ValueSet`.
        // It's not entirely clear why it is private. See this issue:
        // https://github.com/tokio-rs/tracing/issues/2363
        let values = fields.value_set(&values);

        tracing::Event::dispatch(metadata, &values);
    }
}

impl TracingTimer {
    /// Create a new `TracingTimer` at the `debug` log level.
    pub fn new_debug(
        callsite: &'static DefaultCallsite,
        id: String,
        level: tracing::Level,
    ) -> Self {
        Self { id, callsite, start: Instant::now(), level }
    }
}

/// Macro to create a RAII timer that will log a `tracing` event once it's
/// dropped.
///
/// The tracing level can be specified as a first argument, but it's optional.
/// If it's missing, this will use the debug level.
#[macro_export]
macro_rules! timer {
    ($level:expr, $string:expr) => {{
        static __CALLSITE: tracing::callsite::DefaultCallsite = tracing::callsite2! {
            name: tracing::__macro_support::concat!(
                "event ",
                file!(),
                ":",
                line!(),
            ),
            kind: tracing::metadata::Kind::EVENT,
            target: module_path!(),
            level: $level,
            fields: []
        };

        $crate::tracing_timer::TracingTimer::new_debug(&__CALLSITE, $string.into(), $level)
    }};

    ($string:expr) => {
        timer!(tracing::Level::DEBUG, $string)
    };
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_arch = "wasm32"))]
    #[matrix_sdk_test::async_test]
    async fn test_timer_name() {
        use tracing::{span, Level};

        tracing::warn!("Starting test...");

        mod time123 {
            pub async fn run() {
                let _timer_guard = timer!(tracing::Level::DEBUG, "test");
                tokio::time::sleep(instant::Duration::from_millis(123)).await;
                // Displays: 2023-08-25T15:18:31.169498Z DEBUG
                // matrix_sdk_common::tracing_timer::tests: test finished in
                // 124ms
            }
        }

        time123::run().await;

        let span = span!(Level::DEBUG, "le 256ms span");
        let _guard = span.enter();

        let _timer_guard = timer!("in span");
        tokio::time::sleep(instant::Duration::from_millis(256)).await;

        tracing::warn!("Test about to finish.");
        // Displays: 2023-08-25T15:18:31.427070Z DEBUG le 256ms span:
        // matrix_sdk_common::tracing_timer::tests: in span finished in 257ms
    }
}
