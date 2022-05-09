//! Build script for the `matrix-sdk-statestore-sql` crate.

// Check for feature selection mistakes
#[cfg(not(any(feature = "native-tls", feature = "rustls")))]
compile_error!("You must enable either the `native-tls` or `rustls` feature");
#[cfg(all(feature = "native-tls", feature = "rustls"))]
compile_error!("You cannot enable both the `native-tls` and `rustls` features");
#[cfg(not(any(feature = "postgres", feature = "mysql", feature = "sqlite")))]
compile_error!("You must enable at least one database backend feature!");

/// The build script
fn main() {
    // trigger recompilation when a new migration is added
    println!("cargo:rerun-if-changed=migrations");
}
