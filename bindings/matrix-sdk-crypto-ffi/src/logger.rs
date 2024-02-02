use std::sync::{Arc, Mutex};
use std::io::{Result, Write};
use tracing_subscriber::fmt::MakeWriter;
use tracing_core::{Level, Metadata};

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
    level: Level,
}

impl Write for LoggerWrapper {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let message = String::from_utf8_lossy(buf).to_string();
        match self.level {
            Level::ERROR => self.inner.lock().unwrap().log_error(message, String::new()),
            Level::WARN => self.inner.lock().unwrap().log_warn(message, String::new()),
            Level::INFO => self.inner.lock().unwrap().log_info(message, String::new()),
            Level::DEBUG => self.inner.lock().unwrap().log_debug(message, String::new()),
            Level::TRACE => {
                // Assuming log_trace method exists in your Logger trait
                // self.inner.lock().unwrap().log_trace(message, String::new())
                // Commented out because log_trace is not defined in Logger trait
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<()> {
        // You can implement flushing logic here if needed
        Ok(())
    }
}

impl MakeWriter<'_> for LoggerWrapper {
    type Writer = LoggerWrapper;

    fn make_writer(&self) -> Self::Writer {
        Self {
            inner: self.inner.clone(),
            level: Level::DEBUG,
        }
    }

    fn make_writer_for(&self, meta: &Metadata<'_>) -> Self::Writer {
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
        inner: Arc::new(Mutex::new(logger)),
        level: Level::DEBUG,
    };

    // Code to set up the logger with the provided filter
    // Example:
    // let filter = EnvFilter::from_default_env()
    //     .add_directive(Level::TRACE.into());
}

