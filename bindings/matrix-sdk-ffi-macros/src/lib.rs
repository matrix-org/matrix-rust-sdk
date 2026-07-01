// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use proc_macro::TokenStream;
use quote::quote;
use syn::{ImplItem, Item, TraitItem};

/// Attribute to specify the async runtime parameter for the `uniffi`
/// export macros if there any `async fn`s in the input.
#[proc_macro_attribute]
pub fn export(attr: TokenStream, input: TokenStream) -> TokenStream {
    do_export(attr.into(), input.into()).into()
}

fn do_export(
    attr: proc_macro2::TokenStream,
    input: proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    let has_async_fn = |item| {
        if let Item::Fn(fun) = &item {
            if fun.sig.asyncness.is_some() {
                return true;
            }
        } else if let Item::Impl(blk) = &item {
            for item in &blk.items {
                if let ImplItem::Fn(fun) = item
                    && fun.sig.asyncness.is_some()
                {
                    return true;
                }
            }
        } else if let Item::Trait(blk) = &item {
            for item in &blk.items {
                if let TraitItem::Fn(fun) = item
                    && fun.sig.asyncness.is_some()
                {
                    return true;
                }
            }
        }

        false
    };

    let res = match syn::parse2(input.clone()) {
        Ok(item) => match has_async_fn(item) {
            true => {
                quote! {
                  #[cfg_attr(target_family = "wasm", uniffi::export(#attr))]
                  #[cfg_attr(not(target_family = "wasm"), uniffi::export(async_runtime = "tokio", #attr))]
                }
            }
            false => quote! { #[uniffi::export(#attr)] },
        },
        Err(e) => e.into_compile_error(),
    };

    quote! {
        #res
        #input
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::*;

    #[test]
    fn export_adds_uniffi_attribute_to_struct() {
        let attr = quote! { #[export] };

        let input = quote! {
            struct MyStruct {}
        };

        let output = do_export(attr, input);

        assert_eq!(
            output.to_string(),
            quote! {
                #[uniffi::export(#[export])]
                struct MyStruct {}
            }
            .to_string()
        );
    }

    #[test]
    fn export_adds_uniffi_attribute_to_struct_impl() {
        let attr = quote! { #[export] };

        let input = quote! {
            impl MyStruct {
                fn foo() {}
            }
        };

        let output = do_export(attr, input);

        assert_eq!(
            output.to_string(),
            quote! {
                #[uniffi::export(#[export])]
                impl MyStruct {
                    fn foo() {}
                }
            }
            .to_string()
        );
    }

    #[test]
    fn export_adds_tokio_to_nonwasm_impl_export() {
        let attr = quote! { #[export] };

        let input = quote! {
            impl MyStruct {
                async fn foo() {}
            }
        };

        let output = do_export(attr, input);

        assert_eq!(
            output.to_string(),
            quote! {
                #[cfg_attr(target_family = "wasm", uniffi::export(#[export]))]
                #[cfg_attr(not(target_family = "wasm"), uniffi::export(async_runtime = "tokio" , #[export]))]
                impl MyStruct {
                    async fn foo() {}
                }
            }
            .to_string()
        );
    }

    #[test]
    fn export_preserves_fn_attributes_when_no_async_fns() {
        let attr = quote! { #[export] };

        let input = quote! {
            impl MyStruct {
                #[cfg(feature = "myfeature")]
                fn bar() {}
            }
        };

        let output = do_export(attr, input);

        assert_eq!(
            output.to_string(),
            quote! {
                #[uniffi::export(#[export])]
                impl MyStruct {
                    #[cfg(feature = "myfeature")]
                    fn bar() {}
                }
            }
            .to_string()
        );
    }

    #[test]
    fn export_preserves_fn_attributes_even_with_async_fns() {
        let attr = quote! { #[export] };

        let input = quote! {
            impl MyStruct {
                async fn foo() {}
                #[cfg(feature = "myfeature")]
                fn bar() {}
            }
        };

        let output = do_export(attr, input);

        assert_eq!(
            output.to_string(),
            quote! {
                #[cfg_attr(target_family = "wasm", uniffi::export(#[export]))]
                #[cfg_attr(not(target_family = "wasm"), uniffi::export(async_runtime = "tokio" , #[export]))]
                impl MyStruct {
                    async fn foo() {}
                    #[cfg(feature = "myfeature")]
                    fn bar() {}
                }
            }
            .to_string()
        );
    }
}
