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
use syn::{spanned::Spanned as _, ImplItem, Item};

/// Attribute to always specify the async runtime parameter for the `uniffi`
/// export macros.
#[proc_macro_attribute]
pub fn export_async(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = proc_macro2::TokenStream::from(item);

    quote! {
        #[uniffi::export(async_runtime = "tokio")]
        #item
    }
    .into()
}

/// Attribute to always specify the async runtime parameter for the `uniffi`
/// export macros.
#[proc_macro_attribute]
pub fn export(attr: TokenStream, item: TokenStream) -> TokenStream {
    let run_checks = || {
        let item: Item = syn::parse(item.clone())?;
        if let Item::Fn(fun) = &item {
            // Fail compilation if the function is async.
            if fun.sig.asyncness.is_some() {
                let error = syn::Error::new(
                    fun.span(),
                    "async function must be exported with #[export_async]",
                );
                return Err(error);
            }
        } else if let Item::Impl(blk) = &item {
            // Fail compilation if at least one function in the impl block is async.
            for item in &blk.items {
                if let ImplItem::Fn(fun) = item {
                    if fun.sig.asyncness.is_some() {
                        let error = syn::Error::new(
                            blk.span(),
                            "impl block with async functions must be exported with #[export_async]",
                        );
                        return Err(error);
                    }
                }
            }
        }

        Ok(())
    };

    let maybe_error =
        if let Err(err) = run_checks() { Some(err.into_compile_error()) } else { None };

    let item = proc_macro2::TokenStream::from(item);
    let attr = proc_macro2::TokenStream::from(attr);

    quote! {
        #maybe_error

        #[uniffi::export(#attr)]
        #item
    }
    .into()
}
