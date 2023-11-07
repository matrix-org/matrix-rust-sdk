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

//! Utilities for `tracing` in JS environments
use std::{
    cell::RefCell,
    collections::HashMap,
    fmt,
    fmt::Debug,
    io,
    rc::Rc,
    sync::atomic::{AtomicU32, Ordering},
};

use tracing::{level_filters::LevelFilter, Level, Metadata};
use tracing_subscriber::fmt::{
    format::{DefaultFields, Format, Pretty},
    MakeWriter, Subscriber,
};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    /// The type of a javascript-side `Logger` object which we can use to log
    /// from the rust side.
    pub type JsLogger;

    #[wasm_bindgen(method)]
    pub fn debug(this: &JsLogger, data: &JsValue);

    #[wasm_bindgen(method)]
    pub fn info(this: &JsLogger, data: &JsValue);

    #[wasm_bindgen(method)]
    pub fn warn(this: &JsLogger, data: &JsValue);

    #[wasm_bindgen(method)]
    pub fn error(this: &JsLogger, data: &JsValue);
}

/// A [`MakeWriter`] which will construct an [`io::Write`] instance that will
/// write to either a [`JsLogger`] or the JavaScript console.
///
/// You can construct this as part of an entire [`Subscriber`], via
/// [`make_tracing_subscriber`]; alternatively, for more control over log format
/// etc., call [`MakeJsLogWriter::new`] or [`MakeJsLogWriter::new_with_logger`]
/// and then feed the result into
/// [`tracing_subscriber::fmt::SubscriberBuilder::with_writer`]. For example:
///
/// ```
/// # use matrix_sdk_common::js_tracing::MakeJsLogWriter;
/// # use tracing_subscriber::util::SubscriberInitExt;
/// let subscriber =
///     tracing_subscriber::fmt().with_writer(MakeJsLogWriter::new()).finish();
/// subscriber.init();
/// ```
#[derive(Debug)]
pub struct MakeJsLogWriter {
    /// The logger to send messages to, or `None` for the console.
    ///
    /// Since [`MakeWriter`]s need to be [`Send`], and references to JS-side
    /// objects are not `Send`, we cannot refer directly to the logger here. In
    /// practice, the lack of `Send` shouldn't be a problem, because there
    /// will only ever be one thread in the WASM environment, but the types
    /// don't know that.
    ///
    /// So, we indirect through a thread-local hashmap instance. Each time we
    /// construct a new `MakeJsLogWriter` backed by a `JsLogger`, we assign
    /// it a unique ID; we then store that ID in the thread-local hashmap.
    /// Then, when we come to use the logger, provided we are really
    /// in the same thread, we can look up the logger in the map.
    logger_id: Option<u32>,
}

thread_local! {
    static LOGGER_MAP: RefCell<HashMap<u32, Rc<JsLogger>>> = RefCell::new(HashMap::new());
}
static NEXT_LOGGER_ID: AtomicU32 = AtomicU32::new(0);

impl MakeJsLogWriter {
    /// Construct a new [`MakeJsLogWriter`] which will make [`io::Write`]
    /// instances that will write to the JavaScript console.
    pub fn new() -> Self {
        Self { logger_id: None }
    }

    /// Construct a new [`MakeJsLogWriter`] which will make [`io::Write`]
    /// instances that will write to the given [`JsLogger`].
    pub fn new_with_logger(logger: JsLogger) -> Self {
        let logger_id = NEXT_LOGGER_ID.fetch_add(1, Ordering::Relaxed);
        let maker = Self { logger_id: Some(logger_id) };
        LOGGER_MAP.with_borrow_mut(|m| m.insert(logger_id, Rc::new(logger)));
        maker
    }

    /// Helper function containing the common parts of
    /// [`MakeJsLogWriter::make_writer`] and
    /// [`MakeJsLogWriter::make_writer_for`].
    ///
    /// Constructs the actual [`JsLogWriter`] once we have figured out what log
    /// level we want.
    fn make_writer_for_level(&self, level: Level) -> JsLogWriter {
        let logger = self.logger_id.map(|logger_id| {
            LOGGER_MAP.with_borrow(|m| m.get(&logger_id).expect("logger id not found").clone())
        });
        JsLogWriter { logger, level }
    }
}

impl Drop for MakeJsLogWriter {
    fn drop(&mut self) {
        if let Some(logger_id) = self.logger_id {
            LOGGER_MAP.with_borrow_mut(|m| m.remove(&logger_id));
        }
    }
}

impl Default for MakeJsLogWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> MakeWriter<'a> for MakeJsLogWriter {
    type Writer = JsLogWriter;

    fn make_writer(&'a self) -> JsLogWriter {
        self.make_writer_for_level(Level::INFO)
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> JsLogWriter {
        self.make_writer_for_level(*meta.level())
    }
}

/// An [`io::Write`] instance that will write to either a [`JsLogger`] or the
/// JavaScript console.
pub struct JsLogWriter {
    /// The logger to send messages to, or `None` for the console.
    logger: Option<Rc<JsLogger>>,

    /// The log level to log messages at.
    level: Level,
}

impl Debug for JsLogWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsLogWriter").field("level", &self.level).finish_non_exhaustive()
    }
}

impl io::Write for JsLogWriter {
    fn write(&mut self, data: &[u8]) -> Result<usize, io::Error> {
        use std::str;
        let message = str::from_utf8(data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
            .trim_end();
        let message = JsValue::from_str(message);

        match &self.logger {
            Some(logger) => write_message_to_logger(self.level, &message, logger.as_ref()),
            None => write_message_to_console(self.level, &message),
        }

        Ok(data.len())
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
}

fn write_message_to_logger(level: Level, message: &JsValue, logger: &JsLogger) {
    match level {
        Level::TRACE | Level::DEBUG => logger.debug(message),
        Level::INFO => logger.info(message),
        Level::WARN => logger.warn(message),
        Level::ERROR => logger.error(message),
    };
}

fn write_message_to_console(level: Level, message: &JsValue) {
    match level {
        Level::TRACE | Level::DEBUG => web_sys::console::debug_1(message),
        Level::INFO => web_sys::console::info_1(message),
        Level::WARN => web_sys::console::warn_1(message),
        Level::ERROR => web_sys::console::error_1(message),
    };
}

/// The type of [`Subscriber`] returned by [`make_tracing_subscriber`]
pub type JsLoggingSubscriber =
    Subscriber<DefaultFields, Format<Pretty, ()>, LevelFilter, MakeJsLogWriter>;

/// Construct a [`tracing::Subscriber`] which will format logs and send them to
/// the Javascript console or the given logging object.
///
/// # Arguments
///
/// * `logger` - if `None`, logs will be sent to the JS console. Otherwise, must
///   be a reference to a javascript object implementing `debug`, `info`, `warn`
///   and `error` methods each taking a single `String` parameter.
pub fn make_tracing_subscriber(logger: Option<JsLogger>) -> JsLoggingSubscriber {
    let make_writer = match logger {
        Some(logger) => MakeJsLogWriter::new_with_logger(logger),
        None => MakeJsLogWriter::new(),
    };

    let format = tracing_subscriber::fmt::format().without_time().pretty();

    tracing_subscriber::fmt()
        .with_max_level(Level::TRACE)
        .with_writer(make_writer)
        .with_ansi(false)
        .event_format(format)
        .finish()
}

#[cfg(test)]
pub(crate) mod tests {
    use matrix_sdk_test::async_test;
    use tracing::{debug, subscriber::with_default};
    use wasm_bindgen::{JsCast, JsValue};

    use crate::js_tracing::make_tracing_subscriber;

    #[async_test]
    async fn test_make_tracing_subscriber() -> Result<(), js_sys::Error> {
        // construct a javascript-side object which will catch our logs
        let logger = js_sys::eval(
            "({
                  debug_calls: [],
                  debug: function (...args) { this.debug_calls.push(args) },
             })",
        )?;

        // set up a tracing subscriber which will write to it
        let subscriber = make_tracing_subscriber(Some(logger.clone().into()));

        // log something to it
        with_default(subscriber, || {
            debug!("Test message");
        });

        // inspect the call log
        let debug_calls: js_sys::Array =
            js_sys::Reflect::get(&logger, &JsValue::from_str("debug_calls"))?
                .dyn_into()
                .expect("debug_calls not an array");

        // should be one call
        assert_eq!(debug_calls.length(), 1, "Expected 1 call, got {}", debug_calls.length());

        // with one argument
        let call_args: js_sys::Array =
            debug_calls.get(0).dyn_into().expect("call_args not an array");
        assert_eq!(call_args.length(), 1, "Expected 1 argument, got {}", call_args.length());

        let message_string = call_args.get(0).as_string().unwrap();
        let expected_prefix = "  DEBUG matrix_sdk_common::js_tracing::tests: Test message";
        assert!(
            message_string.starts_with(expected_prefix),
            "Expected log message to start with '{}', but was '{}'",
            expected_prefix,
            message_string
        );

        Ok(())
    }
}
