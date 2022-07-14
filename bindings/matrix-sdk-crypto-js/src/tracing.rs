use std::fmt;
#[cfg(feature = "tracing")]
use std::fmt::Write as _;

#[cfg(feature = "tracing")]
use tracing::{
    field::{Field, Visit},
    metadata::LevelFilter,
    Event, Id, Level, Metadata, Subscriber,
};
#[cfg(feature = "tracing")]
use tracing_subscriber::{
    layer::{Context, Layer as TracingLayer},
    prelude::*,
    reload, Registry,
};
use wasm_bindgen::prelude::*;

/// Type to install and to manipulate the tracing layer.
#[wasm_bindgen]
pub struct Tracing {
    #[cfg(feature = "tracing")]
    handle: reload::Handle<Layer, Registry>,
}

impl fmt::Debug for Tracing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tracing").finish_non_exhaustive()
    }
}

#[wasm_bindgen]
impl Tracing {
    /// Check whether the `tracing` feature has been enabled.
    #[wasm_bindgen(js_name = "isAvailable")]
    pub fn is_available() -> bool {
        cfg!(feature = "tracing")
    }

    /// Install the tracing layer.
    ///
    /// **Warning**: It must be installed only **once**, otherwise a
    /// runtime error will be raised.
    pub fn install(_min_level: LoggerLevel) -> Option<Tracing> {
        #[cfg(not(feature = "tracing"))]
        {
            None
        }

        #[cfg(feature = "tracing")]
        {
            let (filter, reload_handle) = reload::Layer::new(Layer::new(_min_level));

            tracing_subscriber::registry().with(filter).init();

            Some(Self { handle: reload_handle })
        }
    }

    /// Re-define the minimum logger level.
    #[cfg(feature = "tracing")]
    #[wasm_bindgen(setter, js_name = "minLevel")]
    pub fn min_level(&self, min_level: LoggerLevel) {
        let _ = self.handle.modify(|layer| layer.min_level = min_level.into());
    }

    /// Turn the logger on, i.e. it emits logs again if it was turned
    /// off.
    #[cfg(feature = "tracing")]
    #[wasm_bindgen(js_name = "turnOn")]
    pub fn turn_on(&self) {
        let _ = self.handle.modify(|layer| layer.turn_on());
    }

    /// Turn the logger off, i.e. it no long emits logs.
    #[cfg(feature = "tracing")]
    #[wasm_bindgen(js_name = "turnOff")]
    pub fn turn_off(&self) {
        let _ = self.handle.modify(|layer| layer.turn_off());
    }
}

/// Logger level.
#[wasm_bindgen]
#[derive(Debug, Clone)]
pub enum LoggerLevel {
    /// `TRACE` level.
    ///
    /// Designate very low priority, often extremely verbose,
    /// information.
    Trace,

    /// `DEBUG` level.
    ///
    /// Designate lower priority information.
    Debug,

    /// `INFO` level.
    ///
    /// Designate useful information.
    Info,

    /// `WARN` level.
    ///
    /// Designate hazardous situations.
    Warn,

    /// `ERROR` level.
    ///
    /// Designate very serious errors.
    Error,
}

#[cfg(feature = "tracing")]
impl From<LoggerLevel> for Level {
    fn from(value: LoggerLevel) -> Self {
        use LoggerLevel::*;

        match value {
            Trace => Self::TRACE,
            Debug => Self::DEBUG,
            Info => Self::INFO,
            Warn => Self::WARN,
            Error => Self::ERROR,
        }
    }
}

#[cfg(feature = "tracing")]
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = "trace")]
    fn log_trace(message: String);

    #[wasm_bindgen(js_namespace = console, js_name = "debug")]
    fn log_debug(message: String);

    #[wasm_bindgen(js_namespace = console, js_name = "info")]
    fn log_info(message: String);

    #[wasm_bindgen(js_namespace = console, js_name = "warn")]
    fn log_warn(message: String);

    #[wasm_bindgen(js_namespace = console, js_name = "log")]
    fn log(message: String);
}

#[cfg(feature = "tracing")]
struct Layer {
    min_level: Level,
    enabled: bool,
}

#[cfg(feature = "tracing")]
impl Layer {
    fn new<L>(min_level: L) -> Self
    where
        L: Into<Level>,
    {
        Self { min_level: min_level.into(), enabled: true }
    }

    fn turn_on(&mut self) {
        self.enabled = true;
    }

    fn turn_off(&mut self) {
        self.enabled = false;
    }
}

#[cfg(feature = "tracing")]
impl<S> TracingLayer<S> for Layer
where
    S: Subscriber,
{
    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        self.enabled && metadata.level() <= &self.min_level
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        if !self.enabled {
            Some(LevelFilter::OFF)
        } else {
            Some(LevelFilter::from_level(self.min_level))
        }
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

        let message = format!("{level} {origin}{recorder}");

        match *level {
            Level::TRACE => log_trace(message),
            Level::DEBUG => log_debug(message),
            Level::INFO => log_info(message),
            Level::WARN => log_warn(message),
            Level::ERROR => log(message),
        }
    }
}

#[cfg(feature = "tracing")]
struct StringVisitor {
    string: String,
}

#[cfg(feature = "tracing")]
impl StringVisitor {
    fn new() -> Self {
        Self { string: String::new() }
    }
}

#[cfg(feature = "tracing")]
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
                let _ = write!(self.string, "\n{} = {:?}", field_name, value);
            }
        }
    }
}

#[cfg(feature = "tracing")]
impl fmt::Display for StringVisitor {
    fn fmt(&self, mut f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.string.is_empty() {
            write!(&mut f, " {}", self.string)
        } else {
            Ok(())
        }
    }
}
