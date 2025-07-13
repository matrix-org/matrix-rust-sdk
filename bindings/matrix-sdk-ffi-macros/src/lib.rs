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
pub fn export(attr: TokenStream, item: TokenStream) -> TokenStream {
    let has_async_fn = |item| {
        if let Item::Fn(fun) = &item {
            if fun.sig.asyncness.is_some() {
                return true;
            }
        } else if let Item::Impl(blk) = &item {
            for item in &blk.items {
                if let ImplItem::Fn(fun) = item {
                    if fun.sig.asyncness.is_some() {
                        return true;
                    }
                }
            }
        } else if let Item::Trait(blk) = &item {
            for item in &blk.items {
                if let TraitItem::Fn(fun) = item {
                    if fun.sig.asyncness.is_some() {
                        return true;
                    }
                }
            }
        }

        false
    };

    let attr2 = proc_macro2::TokenStream::from(attr);
    let item2 = proc_macro2::TokenStream::from(item.clone());

    let res = match syn::parse(item) {
        Ok(item) => match has_async_fn(item) {
            true => {
                quote! {
                  #[cfg_attr(target_family = "wasm", uniffi::export(#attr2))]
                  #[cfg_attr(not(target_family = "wasm"), uniffi::export(async_runtime = "tokio", #attr2))]
                }
            }
            false => quote! { #[uniffi::export(#attr2)] },
        },
        Err(e) => e.into_compile_error(),
    };

    quote! {
        #res
        #item2
    }
    .into()
}
