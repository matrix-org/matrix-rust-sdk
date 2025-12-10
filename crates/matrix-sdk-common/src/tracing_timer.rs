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

use ruma::time::Instant;
use tracing::{Callsite as _, callsite::DefaultCallsite};

/// A named RAII that will show on `Drop` how long its covered section took to
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
        let elapsed = self.start.elapsed();

        let enabled = tracing::level_enabled!(self.level) && {
            let interest = self.callsite.interest();
            !interest.is_never()
                && tracing::__macro_support::__is_enabled(self.callsite.metadata(), interest)
        };

        if !enabled {
            return;
        }

        let message = format!("_{}_ finished in {:?}", self.id, elapsed);

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
    /// Create a new `TracingTimer`.
    pub fn new(callsite: &'static DefaultCallsite, id: String, level: tracing::Level) -> Self {
        Self { id, callsite, start: Instant::now(), level }
    }
}

/// Macro to create a RAII timer that will log on `Drop` how long its covered
/// section took to execute.
///
/// The tracing level can be specified as a first argument, but it's optional.
/// If it's missing, this will use the debug level.
///
/// ```rust,no_run
/// # fn do_long_computation(_x: u32) {}
/// # fn main() {
/// use matrix_sdk_common::timer;
///
/// // It's possible to specify the tracing level we want to be used for the log message on drop.
/// {
///     let _timer = timer!(tracing::Level::TRACE, "do long computation");
///     // But it's optional; by default it's set to `DEBUG`.
///     let _debug_timer = timer!("do long computation but time it in DEBUG");
///     // The macro doesn't support parameters, use `format!()`.
///     let _other_timer = timer!(tracing::Level::TRACE, format!("do long computation with parameter = {}", 123));
///     do_long_computation(123);
/// } // The log statements will happen here.
/// # }
/// ```
#[macro_export]
macro_rules! timer {
    ($level:expr, $string:expr $(,)? ) => {{
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

        $crate::tracing_timer::TracingTimer::new(&__CALLSITE, $string.into(), $level)
    }};

    ($string:expr) => {
        $crate::timer!(tracing::Level::DEBUG, $string)
    };
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_family = "wasm"))]
    #[matrix_sdk_test_macros::async_test]
    async fn test_timer_name() {
        use tracing::{Level, span};

        tracing::warn!("Starting test...");

        mod time123 {
            pub async fn run() {
                let _timer_guard = timer!(tracing::Level::DEBUG, "test");
                tokio::time::sleep(ruma::time::Duration::from_millis(123)).await;
                // Displays: 2023-08-25T15:18:31.169498Z DEBUG
                // matrix_sdk_common::tracing_timer::tests: _test_ finished in
                // 124ms
            }
        }

        time123::run().await;

        let span = span!(Level::DEBUG, "le 256ms span");
        let _guard = span.enter();

        let _timer_guard = timer!("in span");
        tokio::time::sleep(ruma::time::Duration::from_millis(256)).await;

        tracing::warn!("Test about to finish.");
        // Displays: 2023-08-25T15:18:31.427070Z DEBUG le 256ms span:
        // matrix_sdk_common::tracing_timer::tests: in span finished in 257ms
    }
}
