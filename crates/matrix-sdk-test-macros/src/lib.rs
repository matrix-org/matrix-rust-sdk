use proc_macro::TokenStream;
use quote::ToTokens;
use syn::parse_macro_input;
use quote::{quote, format_ident};
use syn;

/// Attribute to use `wasm_bindgen_test` for wasm32 targets and `tokio::test`
/// for everything else
#[proc_macro_attribute]
pub fn async_test(_attr: TokenStream, item: TokenStream) -> TokenStream  {

    let fun = parse_macro_input!(item as syn::ItemFn);
    if fun.sig.output == syn::ReturnType::Default {
        let attrs = r#"
            #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
            #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
        "#;

        let mut out: TokenStream = attrs.parse().expect("Static works");
        let inner : TokenStream = fun.into_token_stream().into();
        out.extend(inner);
        return out
    }

    let attrs = r#"
        #[cfg_attr(not(target_arch = "wasm32"), tokio::test)]
    "#;

    let mut out: TokenStream = attrs.parse().expect("Static works.");

    let mut outer = fun.clone();
    let fn_name = fun.sig.ident.clone();
    let fn_call : TokenStream = if fun.sig.asyncness.is_some() {
        quote!{
            {
                assert!(#fn_name().await.is_ok());
            }
        }
    } else {
        quote!{
            {
                assert!(#fn_name().is_ok());
            }
        }
    }.into();
    outer.sig.output = syn::ReturnType::Default;
    outer.sig.ident = format_ident!("{}_outer", fun.sig.ident);
    outer.block = Box::new(parse_macro_input!(fn_call as syn::Block));


    let inner : TokenStream = fun.into_token_stream().into();
    out.extend(inner);

    let attrs = r#"
        #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    "#;
    let outer_attrs: TokenStream = attrs.parse().expect("Static works.");
    let of : TokenStream = outer.into_token_stream().into();
    out.extend(outer_attrs);
    out.extend(of);

    out
}

// Attribute to use `wasm_bindgen_test` for wasm32 targets and `tokio::test`
// for everything else
// #[cfg(not(target_arch = "wasm32"))]
// #[proc_macro_attribute]
// pub fn async_test(_attr: TokenStream, item: TokenStream) -> TokenStream  {
// }
