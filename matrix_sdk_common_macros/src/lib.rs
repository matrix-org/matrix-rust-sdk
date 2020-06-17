use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, parse_quote, ItemTrait};

/// Attribute to use `Send + Sync` for everything but wasm32
#[proc_macro_attribute]
pub fn send_sync(_attr: TokenStream, input: TokenStream) -> TokenStream {
    // Parse the input tokens into a syntax tree
    let mut input = parse_macro_input!(input as ItemTrait);

    let send_trait_bound = parse_quote!(std::marker::Send);
    let sync_trait_bound = parse_quote!(std::marker::Sync);
    input.supertraits.push(send_trait_bound);
    input.supertraits.push(sync_trait_bound);

    TokenStream::from(quote!(#input))
}

/// A wasm32 compatible wrapper for the async_trait::async_trait macro
#[proc_macro_attribute]
pub fn async_trait(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = r#"
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    "#;

    let mut out: TokenStream = attrs.parse().unwrap();
    out.extend(item);
    out
}
