use proc_macro::TokenStream;
use quote::{ToTokens, format_ident, quote};
use syn::parse_macro_input;

/// Attribute to use `wasm_bindgen_test` for wasm32 targets and `tokio::test`
/// for everything else with async-support and custom result-types
#[proc_macro_attribute]
pub fn async_test(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let fun = parse_macro_input!(item as syn::ItemFn);

    if !fun.sig.ident.to_string().starts_with("test_") {
        panic!("test function names must start with test_");
    }

    // on the regular return-case, we can just use cfg_attr and quit early
    if fun.sig.output == syn::ReturnType::Default {
        let attrs = r#"
            #[cfg_attr(not(target_family = "wasm"), tokio::test)]
            #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
        "#;

        let mut out: TokenStream = attrs.parse().expect("Static works");
        let inner: TokenStream = fun.into_token_stream().into();
        out.extend(inner);
        return out;
    }

    // on the more complicated case, where we have some `->`-Result-Arrow
    // `wasm_bindgen_test` doesn't yet support
    // - see https://github.com/rustwasm/wasm-bindgen/issues/2565 -
    // we split the attribution into to functions: one with the original
    // to be attributed for non-wasm, and then a second outer-wrapper function
    // that calls the first in wasm32 cases.

    let attrs = r#"
        #[cfg_attr(not(target_family = "wasm"), tokio::test)]
    "#;

    let mut out: TokenStream = attrs.parse().expect("Static works.");

    let mut outer = fun.clone();
    let fn_name = fun.sig.ident.clone();
    let fn_call: TokenStream = if fun.sig.asyncness.is_some() {
        quote! {
            {
                let res = #fn_name().await;
                assert!(res.is_ok(), "{:?}", res);
            }
        }
    } else {
        quote! {
            {
                let res = #fn_name();
                assert!(res.is_ok(), "{:?}", res);
            }
        }
    }
    .into();
    outer.sig.output = syn::ReturnType::Default;
    outer.sig.ident = format_ident!("{}_outer", fun.sig.ident);
    outer.block = Box::new(parse_macro_input!(fn_call as syn::Block));

    let inner: TokenStream = fun.into_token_stream().into();
    out.extend(inner);

    let attrs = r#"
        #[cfg(target_family = "wasm")]
        #[wasm_bindgen_test::wasm_bindgen_test]
    "#;
    let outer_attrs: TokenStream = attrs.parse().expect("Static works.");
    let of: TokenStream = outer.into_token_stream().into();
    out.extend(outer_attrs);
    out.extend(of);

    out
}
