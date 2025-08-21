/// Initialize a tracing subscriber if the target architecture is not WASM.
///
/// Uses a sensible default filter that can be overridden through the `RUST_LOG`
/// environment variable and runs once before all tests by using the [`ctor`]
/// crate.
///
/// Invoke this macro once per compilation unit (`lib.rs`, `tests/*.rs`,
/// `tests/*/main.rs`).
#[macro_export]
macro_rules! init_tracing_for_tests {
    () => {
        #[cfg(not(target_family = "wasm"))]
        #[$crate::__macro_support::ctor]
        fn init_logging() {
            use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
            use $crate::__macro_support::tracing_subscriber;

            tracing_subscriber::registry()
                .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    // Output is only printed for failing tests, but still we shouldn't overload
                    // the output with unnecessary info. When debugging a specific test, it's easy
                    // to override this default by setting the `RUST_LOG` environment variable.
                    //
                    // Since tracing_subscriber does prefix matching, the `matrix_sdk=` directive
                    // takes effect for all the main crates (`matrix_sdk_base`, `matrix_sdk_crypto`
                    // and so on).
                    "info,matrix_sdk=debug".into()
                }))
                .with(tracing_subscriber::fmt::layer().with_test_writer())
                .init();
        }
    };
}

#[doc(hidden)]
pub mod __macro_support {
    #[cfg(not(target_family = "wasm"))]
    pub use ctor::ctor;
    #[cfg(not(target_family = "wasm"))]
    pub use tracing_subscriber;
}
