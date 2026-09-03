use std::{
    io::{Result, Write},
    sync::{Arc, Mutex},
};

use tracing_core::{Level, Metadata};
use tracing_subscriber::{EnvFilter, fmt::MakeWriter};

/// Trait that can be used to forward Rust logs over FFI to a language specific
/// logger.
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait Logger: Send {
    /// Called every time the Rust side wants to post a debug log line.
    fn log_debug(&self, log_line: String);
    /// Called every time the Rust side wants to post an error log line.
    fn log_error(&self, log_line: String);
    /// Called every time the Rust side wants to post an info log line.
    fn log_info(&self, log_line: String);
    /// Called every time the Rust side wants to post a trace log line.
    fn log_trace(&self, log_line: String);
    /// Called every time the Rust side wants to post a warn log line.
    fn log_warn(&self, log_line: String);
}

impl Write for LoggerWrapper {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let data = String::from_utf8_lossy(buf).to_string();
        let lock = self.inner.lock().unwrap();

        match self.level {
            Level::DEBUG => lock.log_debug(data),
            Level::ERROR => lock.log_error(data),
            Level::INFO => lock.log_info(data),
            Level::TRACE => lock.log_trace(data),
            Level::WARN => lock.log_warn(data),
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl MakeWriter<'_> for LoggerWrapper {
    type Writer = LoggerWrapper;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }

    fn make_writer_for(&self, meta: &Metadata<'_>) -> Self::Writer {
        LoggerWrapper { inner: self.inner.clone(), level: *meta.level() }
    }
}

#[derive(Clone)]
pub struct LoggerWrapper {
    inner: Arc<Mutex<Box<dyn Logger>>>,
    level: Level,
}

/// Set the logger that should be used to forward Rust logs over FFI.
#[matrix_sdk_ffi_macros::export]
pub fn set_logger(logger: Box<dyn Logger>) {
    let logger = LoggerWrapper { inner: Arc::new(Mutex::new(logger)), level: Level::DEBUG };

    let filter = EnvFilter::from_default_env()
        .add_directive(
            "matrix_sdk_crypto=trace".parse().expect("Can't parse logging filter directive"),
        )
        .add_directive(
            "matrix_sdk_sqlite=debug".parse().expect("Can't parse logging filter directive"),
        );

    let _ = tracing_subscriber::fmt()
        .with_writer(logger)
        .with_env_filter(filter)
        .with_ansi(false)
        .without_time()
        .try_init();
}
