use std::{collections::BTreeMap, sync::Mutex};

use once_cell::sync::OnceCell;
use tracing::{callsite::DefaultCallsite, field::FieldSet, Callsite};
use tracing_core::{identify_callsite, metadata::Kind as MetadataKind};

/// Log an event at the given callsite (file, line and column).
///
/// The target should be something like a module path, and can be referenced in
/// the filter string given to `setup_tracing`. `level` and `target` for a
/// callsite are fixed at the first `log_event` call for that callsite and can
/// not be changed afterwards, i.e. the level and target passed for second and
/// following `log_event`s with the same callsite will be ignored.
///
/// This function leaks a little bit of memory for each unique callsite it is
/// called with. Please make sure that the number of different
/// `(file, line, column)` tuples that this can be called with is fixed in the
/// final executable.
#[uniffi::export]
fn log_event(
    file: String,
    line: u32,
    column: u32,
    level: LogLevel,
    target: String,
    message: String,
) {
    static CALLSITES: Mutex<BTreeMap<Location, &'static DefaultCallsite>> =
        Mutex::new(BTreeMap::new());
    let loc = Location::new(file, line, column);
    let callsite = get_or_init_metadata(&CALLSITES, loc, level, target, |loc| {
        (format!("event {}:{}", loc.file, loc.line), &["message"], MetadataKind::EVENT)
    });
    let metadata = callsite.metadata();

    if span_or_event_enabled(callsite) {
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

type FieldNames = &'static [&'static str];

fn get_or_init_metadata(
    mutex: &Mutex<BTreeMap<Location, &'static DefaultCallsite>>,
    loc: Location,
    level: LogLevel,
    target: String,
    get_details: impl FnOnce(&Location) -> (String, FieldNames, MetadataKind),
) -> &'static DefaultCallsite {
    mutex.lock().unwrap().entry(loc).or_insert_with_key(|loc| {
        let (name, field_names, span_kind) = get_details(loc);
        let callsite = Box::leak(Box::new(LateInitCallsite(OnceCell::new())));
        let metadata = Box::leak(Box::new(tracing::Metadata::new(
            Box::leak(name.into_boxed_str()),
            Box::leak(target.into_boxed_str()),
            level.to_tracing_level(),
            Some(Box::leak(Box::from(loc.file.as_str()))),
            Some(loc.line),
            None, // module path
            FieldSet::new(field_names, identify_callsite!(callsite)),
            span_kind,
        )));
        callsite.0.try_insert(DefaultCallsite::new(metadata)).expect("callsite was not set before")
    })
}

fn span_or_event_enabled(callsite: &'static DefaultCallsite) -> bool {
    use tracing::{
        dispatcher,
        level_filters::{LevelFilter, STATIC_MAX_LEVEL},
    };

    let meta = callsite.metadata();
    let level = *meta.level();

    if level > STATIC_MAX_LEVEL || level > LevelFilter::current() {
        false
    } else {
        let interest = callsite.interest();
        interest.is_always()
            || !interest.is_never() && dispatcher::get_default(|default| default.enabled(meta))
    }
}

pub struct Span(tracing::Span);

impl Span {
    /// Create a span originating at the given callsite (file, line and column).
    ///
    /// The target should be something like a module path, and can be referenced
    /// in the filter string given to `setup_tracing`. `level` and `target`
    /// for a callsite are fixed at the first creation of a span for that
    /// callsite and can not be changed afterwards, i.e. the level and
    /// target passed for second and following creation of a span with the same
    /// callsite will be ignored.
    ///
    /// This function leaks a little bit of memory for each unique callsite it
    /// is called with. Please make sure that the number of different
    /// `(file, line, column)` tuples that this can be called with is fixed in
    /// the final executable.
    ///
    /// For a span to have an effect, you must `.enter()` it at the start of a
    /// logical unit of work and `.exit()` it at the end of the same (including
    /// on failure). Entering registers the span in thread-local storage, so
    /// future calls to `log_event` on the same thread are able to attach the
    /// events they create to the span, exiting unregisters it. For this to
    /// work, exiting a span must be done on the same thread where it was
    /// entered. It is possible to enter a span on multiple threads, in which
    /// case it should also be exited on all of them individually; that is,
    /// unless you *want* the span to be attached to all further events created
    /// on that thread.
    pub fn new(
        file: String,
        line: u32,
        column: u32,
        level: LogLevel,
        target: String,
        name: String,
    ) -> Self {
        static CALLSITES: Mutex<BTreeMap<Location, &'static DefaultCallsite>> =
            Mutex::new(BTreeMap::new());
        let loc = Location::new(file, line, column);
        let callsite = get_or_init_metadata(&CALLSITES, loc, level, target, |_loc| {
            (name, &[], MetadataKind::SPAN)
        });
        let metadata = callsite.metadata();

        let span = if span_or_event_enabled(callsite) {
            // This function is hidden from docs, but we have to use it (see above).
            let values = metadata.fields().value_set(&[]);
            tracing::Span::new(metadata, &values)
        } else {
            tracing::Span::none()
        };

        Span(span)
    }

    pub fn current() -> Self {
        Self(tracing::Span::current())
    }
}

#[uniffi::export]
impl Span {
    fn enter(&self) {
        self.0.with_subscriber(|(id, dispatch)| dispatch.enter(id));
    }

    fn exit(&self) {
        self.0.with_subscriber(|(id, dispatch)| dispatch.exit(id));
    }

    fn is_none(&self) -> bool {
        self.0.is_none()
    }
}

pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn to_tracing_level(&self) -> tracing::Level {
        match self {
            LogLevel::Error => tracing::Level::ERROR,
            LogLevel::Warn => tracing::Level::WARN,
            LogLevel::Info => tracing::Level::INFO,
            LogLevel::Debug => tracing::Level::DEBUG,
            LogLevel::Trace => tracing::Level::TRACE,
        }
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Location {
    file: String,
    line: u32,
    column: u32,
}

impl Location {
    fn new(file: String, line: u32, column: u32) -> Self {
        Self { file, line, column }
    }
}

struct LateInitCallsite(OnceCell<DefaultCallsite>);

impl Callsite for LateInitCallsite {
    fn set_interest(&self, interest: tracing_core::Interest) {
        self.0
            .get()
            .expect("Callsite impl must not be used before initialization")
            .set_interest(interest)
    }

    fn metadata(&self) -> &tracing::Metadata<'_> {
        self.0.get().expect("Callsite impl must not be used before initialization").metadata()
    }
}
