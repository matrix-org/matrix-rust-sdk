use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use once_cell::sync::OnceCell;
use tracing::{callsite::DefaultCallsite, field::FieldSet, Callsite};
use tracing_core::{identify_callsite, metadata::Kind as MetadataKind};

type StaticMetadata = &'static tracing::Metadata<'static>;

#[uniffi::export]
fn log_event(
    file: String,
    line: u32,
    column: u32,
    level: LogLevel,
    target: String,
    message: String,
) {
    static METADATA: Mutex<BTreeMap<Location, StaticMetadata>> = Mutex::new(BTreeMap::new());
    let loc = Location::new(file, line, column);
    let metadata = get_or_init_metadata(&METADATA, loc, level, target, |loc| {
        (format!("event {}:{}", loc.file, loc.line), &["message"], MetadataKind::EVENT)
    });

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

#[uniffi::export]
fn make_span(
    file: String,
    line: u32,
    column: u32,
    level: LogLevel,
    target: String,
    name: String,
) -> Arc<Span> {
    static METADATA: Mutex<BTreeMap<Location, StaticMetadata>> = Mutex::new(BTreeMap::new());
    let loc = Location::new(file, line, column);
    let metadata =
        get_or_init_metadata(&METADATA, loc, level, target, |_loc| (name, &[], MetadataKind::SPAN));

    // This function is hidden from docs, but we have to use it (see above).
    let values = metadata.fields().value_set(&[]);
    Arc::new(Span(tracing::Span::new(metadata, &values)))
}

type FieldNames = &'static [&'static str];

fn get_or_init_metadata(
    mutex: &Mutex<BTreeMap<Location, StaticMetadata>>,
    loc: Location,
    level: LogLevel,
    target: String,
    get_details: impl FnOnce(&Location) -> (String, FieldNames, MetadataKind),
) -> StaticMetadata {
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
        callsite.0.set(DefaultCallsite::new(metadata)).expect("callsite was not set before");
        metadata
    })
}

#[derive(uniffi::Object)]
pub struct Span(tracing::Span);

#[uniffi::export]
impl Span {
    fn enter(&self) {
        self.0.with_subscriber(|(id, dispatch)| dispatch.enter(id));
    }

    fn exit(&self) {
        self.0.with_subscriber(|(id, dispatch)| dispatch.exit(id));
    }
}

#[derive(uniffi::Enum)]
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
