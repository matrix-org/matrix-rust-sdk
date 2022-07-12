use std::fmt::{self, Write as _};

use tracing::{
    field::{Field, Visit},
    Event, Id, Level, Metadata, Subscriber,
};
use tracing_subscriber::{
    layer::{Context, Layer as TracingLayer},
    prelude::*,
};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
#[derive(Debug)]
pub enum LoggerLevel {
    Debug,
    Info,
    Warn,
    Trace,
    Error,
}

impl From<LoggerLevel> for Level {
    fn from(value: LoggerLevel) -> Self {
        use LoggerLevel::*;

        match value {
            Debug => Self::DEBUG,
            Info => Self::INFO,
            Warn => Self::WARN,
            Trace => Self::TRACE,
            Error => Self::ERROR,
        }
    }
}

#[wasm_bindgen(js_name = "useLogger")]
pub fn use_logger(level: LoggerLevel) {
    tracing_subscriber::registry().with(Layer::new(level)).init();
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = "debug")]
    fn log_debug(message: String);

    #[wasm_bindgen(js_namespace = console, js_name = "info")]
    fn log_info(message: String);

    #[wasm_bindgen(js_namespace = console, js_name = "warn")]
    fn log_warn(message: String);

    #[wasm_bindgen(js_namespace = console, js_name = "trace")]
    fn log_trace(message: String);

    #[wasm_bindgen(js_namespace = console, js_name = "log")]
    fn log(message: String);

    #[wasm_bindgen(js_namespace = console, js_name= "group")]
    fn log_group_open(label: String);

    #[wasm_bindgen(js_namespace = console, js_name= "groupEnd")]
    fn log_group_close();
}

struct Layer {
    level: Level,
}

impl Layer {
    fn new<L>(level: L) -> Self
    where
        L: Into<Level>,
    {
        Self { level: level.into() }
    }
}

impl<S> TracingLayer<S> for Layer
where
    S: Subscriber,
{
    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        metadata.level() >= &self.level
    }

    fn on_enter(&self, id: &Id, _: Context<'_, S>) {
        log_group_open(format!("t{:x}", id.into_u64()));
    }

    fn on_exit(&self, _: &Id, _: Context<'_, S>) {
        log_group_close();
    }

    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut recorder = StringVisitor::new();
        event.record(&mut recorder);
        let metadata = event.metadata();
        let level = metadata.level();

        let origin = metadata
            .file()
            .and_then(|file| metadata.line().map(|ln| format!("{}:{}", file, ln)))
            .unwrap_or_default();

        let message = format!("{level} {origin} {recorder}");

        match *level {
            Level::DEBUG => log_debug(message),
            Level::INFO => log_info(message),
            Level::WARN => log_warn(message),
            Level::TRACE => log_trace(message),
            Level::ERROR => log(message),
        }
    }
}

struct StringVisitor {
    string: String,
    displaying_arguments: bool,
}

impl StringVisitor {
    fn new() -> Self {
        Self { string: String::new(), displaying_arguments: false }
    }
}

impl Visit for StringVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        match field.name() {
            "message" => {
                if !self.string.is_empty() {
                    self.string.push('\n');
                }

                let _ = write!(self.string, "{:?}", value);
            }

            field_name => {
                if self.displaying_arguments {
                    self.string.push('\n');
                } else {
                    self.string.push(' ');
                    self.displaying_arguments = true;
                }

                let _ = write!(self.string, "{} = {:?}", field_name, value);
            }
        }
    }
}

impl fmt::Display for StringVisitor {
    fn fmt(&self, mut f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.string.is_empty() {
            write!(&mut f, " {}", self.string)
        } else {
            Ok(())
        }
    }
}
