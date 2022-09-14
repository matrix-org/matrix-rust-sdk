// Copyright 2022 The Matrix.org Foundation C.I.C.
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

#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#![warn(missing_docs, missing_debug_implementations)]
#![allow(clippy::drop_non_drop)] // triggered by wasm_bindgen code

pub mod attachment;
pub mod device;
pub mod encryption;
pub mod events;
mod future;
pub mod identifiers;
pub mod machine;
pub mod olm;
mod macros;
pub mod requests;
pub mod responses;
pub mod store;
pub mod sync_events;
mod tracing;
pub mod types;
pub mod verification;
pub mod vodozemac;

use js_sys::{Object, Reflect};
use wasm_bindgen::{convert::RefFromWasmAbi, prelude::*};

/// Run some stuff when the Wasm module is instantiated.
///
/// Right now, it does the following:
///
/// * Redirect Rust panics to JavaScript console.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// A really hacky and dirty code to downcast a `JsValue` to `T:
/// RefFromWasmAbi`, inspired by
/// https://github.com/rustwasm/wasm-bindgen/issues/2231#issuecomment-656293288.
///
/// The returned value is likely to be a `wasm_bindgen::__ref::Ref<T>`.
fn downcast<T>(value: &JsValue, classname: &str) -> Result<T::Anchor, JsError>
where
    T: RefFromWasmAbi<Abi = u32>,
{
    let constructor_name = Object::get_prototype_of(value).constructor().name();

    if constructor_name == classname {
        let pointer = Reflect::get(value, &JsValue::from_str("ptr"))
            .map_err(|_| JsError::new("Failed to read the `JsValue` pointer"))?;
        let pointer = pointer
            .as_f64()
            .ok_or_else(|| JsError::new("Failed to read the `JsValue` pointer as a `f64`"))?
            as u32;

        Ok(unsafe { T::ref_from_abi(pointer) })
    } else {
        Err(JsError::new(&format!(
            "Expect an `{classname}` instance, received `{constructor_name}` instead",
        )))
    }
}
