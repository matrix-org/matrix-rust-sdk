use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

/// A wrapper around `async_trait` that automatically handles Wasm
/// compatibility.
///
/// On Wasm targets, this expands to `#[async_trait(?Send)]` since Wasm is
/// single-threaded. On other targets, this expands to `#[async_trait]` with
/// Send bounds.
///
/// This should not be used directly, it is intended to be re-exported by
/// the `matrix-sdk-common` crate for use in external crates that depend on it.
#[proc_macro_attribute]
pub fn async_trait(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = TokenStream2::from(item);

    // Use the matrix_sdk_common re-export for external crates
    let expanded = quote! {
        #[cfg_attr(target_family = "wasm", ::matrix_sdk_common::async_trait_impl(?Send))]
        #[cfg_attr(not(target_family = "wasm"), ::matrix_sdk_common::async_trait_impl)]
        #item
    };

    TokenStream::from(expanded)
}

/// A wrapper around `async_trait` for use inside matrix-sdk-common that
/// automatically handles Wasm compatibility.
///
/// This version uses `::async_trait::async_trait` directly since
/// matrix-sdk-common has the dependency.
#[proc_macro_attribute]
pub fn async_trait_internal(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = TokenStream2::from(item);

    // Use async_trait directly for internal use in matrix-sdk-common
    let expanded = quote! {
        #[cfg_attr(target_family = "wasm", ::async_trait::async_trait(?Send))]
        #[cfg_attr(not(target_family = "wasm"), ::async_trait::async_trait)]
        #item
    };

    TokenStream::from(expanded)
}
