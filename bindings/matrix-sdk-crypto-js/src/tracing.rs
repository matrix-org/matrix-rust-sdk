use wasm_bindgen::prelude::*;

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
mod inner {
    use std::{
        fmt,
        fmt::Write as _,
        sync::{Arc, Once},
    };

    use tracing::{
        field::{Field, Visit},
        metadata::LevelFilter,
        Event, Level, Metadata, Subscriber,
    };
    use tracing_subscriber::{
        layer::{Context, Layer as TracingLayer},
        prelude::*,
        reload, Registry,
    };

    use super::*;

    type TracingInner = Arc<reload::Handle<Layer, Registry>>;

    /// Type to install and to manipulate the tracing layer.
    #[wasm_bindgen]
    pub struct Tracing {
        handle: TracingInner,
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
            true
        }

        /// Install the tracing layer.
        ///
        /// `Tracing` is a singleton. Once it is installed,
        /// consecutive calls to the constructor will construct a new
        /// `Tracing` object but with the exact same inner
        /// state. Calling the constructor with a new `min_level` will
        /// just update the `min_level` parameter; in that regard, it
        /// is similar to calling the `min_level` method on an
        /// existing `Tracing` object.
        #[wasm_bindgen(constructor)]
        pub fn new(min_level: LoggerLevel) -> Result<Tracing, JsError> {
            static mut INSTALL: Option<TracingInner> = None;
            static INSTALLED: Once = Once::new();

            INSTALLED.call_once(|| {
                let (filter, reload_handle) = reload::Layer::new(Layer::new(min_level.clone()));

                tracing_subscriber::registry().with(filter).init();

                unsafe { INSTALL = Some(Arc::new(reload_handle)) };
            });

            let tracing = Tracing {
                handle: unsafe { INSTALL.as_ref() }
                    .cloned()
                    .expect("`Tracing` has not been installed correctly"),
            };

            // If it's not the first call to `install`, the
            // `min_level` can be different. Let's update it.
            tracing.min_level(min_level);

            Ok(tracing)
        }

        /// Re-define the minimum logger level.
        #[wasm_bindgen(setter, js_name = "minLevel")]
        pub fn min_level(&self, min_level: LoggerLevel) {
            let _ = self.handle.modify(|layer| layer.min_level = min_level.into());
        }

        /// Turn the logger on, i.e. it emits logs again if it was turned
        /// off.
        #[wasm_bindgen(js_name = "turnOn")]
        pub fn turn_on(&self) {
            let _ = self.handle.modify(|layer| layer.turn_on());
        }

        /// Turn the logger off, i.e. it no long emits logs.
        #[wasm_bindgen(js_name = "turnOff")]
        pub fn turn_off(&self) {
            let _ = self.handle.modify(|layer| layer.turn_off());
        }
    }

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

        #[wasm_bindgen(js_namespace = console, js_name = "error")]
        fn log_error(message: String);
    }

    struct Layer {
        min_level: Level,
        enabled: bool,
    }

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
                .and_then(|file| metadata.line().map(|ln| format!("{file}:{ln}")))
                .unwrap_or_default();

            let message = format!("{level} {origin}{recorder}");

            match *level {
                Level::TRACE => log_debug(message),
                Level::DEBUG => log_debug(message),
                Level::INFO => log_info(message),
                Level::WARN => log_warn(message),
                Level::ERROR => log_error(message),
            }
        }
    }

    struct StringVisitor {
        string: String,
    }

    impl StringVisitor {
        fn new() -> Self {
            Self { string: String::new() }
        }
    }

    impl Visit for StringVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            match field.name() {
                "message" => {
                    if !self.string.is_empty() {
                        self.string.push('\n');
                    }

                    let _ = write!(self.string, "{value:?}");
                }

                field_name => {
                    let _ = write!(self.string, "\n{field_name} = {value:?}");
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
}

#[cfg(not(feature = "tracing"))]
mod inner {
    use super::*;

    /// Type to install and to manipulate the tracing layer.
    #[wasm_bindgen]
    #[derive(Debug)]
    pub struct Tracing;

    #[wasm_bindgen]
    impl Tracing {
        /// Check whether the `tracing` feature has been enabled.
        #[wasm_bindgen(js_name = "isAvailable")]
        pub fn is_available() -> bool {
            false
        }

        /// The `tracing` feature is not enabled, so this constructor
        /// will raise an error.
        #[wasm_bindgen(constructor)]
        pub fn new(_min_level: LoggerLevel) -> Result<Tracing, JsError> {
            Err(JsError::new("The `tracing` feature is disabled. Check `Tracing.isAvailable` before constructing `Tracing`"))
        }
    }
}

pub use inner::*;
