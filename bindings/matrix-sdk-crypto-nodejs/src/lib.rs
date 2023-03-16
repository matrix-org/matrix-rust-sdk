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
//#![warn(missing_docs, missing_debug_implementations)]

use napi_derive::napi;

pub mod attachment;
pub mod encryption;
mod errors;
pub mod events;
pub mod identifiers;
pub mod machine;
pub mod olm;
pub mod requests;
pub mod responses;
pub mod sync_events;
#[cfg(feature = "tracing")]
pub mod tracing;
pub mod types;
pub mod vodozemac;

/// Object containing the versions of the Rust libraries we are using.
#[napi]
pub struct Versions {
    /// The version of the vodozemac crate.
    #[napi(getter)]
    pub vodozemac: String,

    /// The version of the matrix-sdk-crypto crate.
    #[napi(getter)]
    pub matrix_sdk_crypto: String,
}

/// Get the versions of the Rust libraries we are using.
#[napi(js_name = "getVersions")]
pub fn get_versions() -> Versions {
    Versions {
        vodozemac: matrix_sdk_crypto::vodozemac::VERSION.to_owned(),
        matrix_sdk_crypto: matrix_sdk_crypto::VERSION.to_owned(),
    }
}

use crate::errors::into_err;
