mod traits;

#[cfg(feature = "native")]
mod native;

// #[cfg(feature = "javascriptcore")]
mod javascriptcore;

// #[cfg(feature = "javascriptcore")]
// pub use javascriptcore::NativeInstance as Instance;
// #[cfg(feature = "native")]
// pub use native::NativeInstance as Instance;

pub type Result<T> = core::result::Result<T, anyhow::Error>;
