use std::{
    io::{Result, Write},
    sync::{Arc, Mutex},
};
use tracing_core::Level;
use tracing_subscriber::{fmt::MakeWriter, EnvFilter};

/// Trait that can be used to forward Rust logs over FFI to a language-specific
/// logger.
#[uniffi::export(callback_interface)]
pub trait Logger: Send {
  /// Called every time the Rust side wants to post a debug log line.
    fn log_debug(&self, message: String, data: String);

    /// Called every time the Rust side wants to post an info log line.
    fn log_info(&self, message: String, data: String);

    /// Called every time the Rust side wants to post a warning log line.
    fn log_warn(&self, message: String, data: String);

    /// Called every time the Rust side wants to post an error log line.
    fn log_error(&self, message: String, data: String);
}
pub struct LoggerWrapper {
    inner: Arc<Mutex<Box<dyn Logger>>>,
    level: tracing_core::Level,
}
impl Write for LoggerWrapper {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let message = String::from_utf8_lossy(buf).to_string();
  match self.level {
            tracing_core::Level::ERROR => self.inner.lock().unwrap().log_error(message, String::new()),
            tracing_core::Level::WARN => self.inner.lock().unwrap().log_warn(message, String::new()),
            tracing_core::Level::INFO => self.inner.lock().unwrap().log_info(message, String::new()),
            tracing_core::Level::DEBUG => self.inner.lock().unwrap().log_debug(message, String::new()),
            tracing_core::Level::TRACE => {
                // Assuming log_trace method exists in your Logger trait
                self.inner.lock().unwrap().log_trace(message, String::new())
            }
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
         // Assuming a default log level of DEBUG
        Self {
            inner: self.inner.clone(),
            level: tracing_core::Level::DEBUG
    }
}

    fn make_writer_for(&self, meta: &tracing_core::Metadata<'_>) -> Self::Writer {
        Self {
            inner: self.inner.clone(),
            level: meta.level().clone(),
        }
    }
}

/// Set the logger that should be used to forward Rust logs over FFI.
#[uniffi::export]
pub fn set_logger(logger: Box<dyn Logger>) {
    let logger = LoggerWrapper { 
        inner: Arc::new(Mutex::new(logger))
        level: tracing_core::Level::DEBUG,  };

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
