use proc_macro::TokenStream;

/// Attribute to use `wasm_bindgen_test` for wasm32 targets and `tokio::test` for everything else
#[proc_macro_attribute]
pub fn async_test(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = r#"
        #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
        #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    "#;

    let mut out: TokenStream = attrs.parse().unwrap();
    out.extend(item);
    out
}
