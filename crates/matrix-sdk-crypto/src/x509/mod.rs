// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! Types and traits for verification of users and devices using X.509 keys and
//! certificates.

mod rust_x509_sign;
mod rust_x509_verify;
mod x509_data;
mod x509_signer;
mod x509_verify;

pub use x509_data::X509Data;
pub use x509_signer::{X509Sign, X509Signer};
pub use x509_verify::{X509Verifier, X509Verify};
