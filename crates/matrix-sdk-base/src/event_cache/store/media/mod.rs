// Copyright 2025 Kévin Commaille
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

//! Types and traits regarding media caching of the event cache store.

mod media_retention_policy;
mod media_service;
#[cfg(any(test, feature = "testing"))]
#[macro_use]
pub mod integration_tests;

#[cfg(any(test, feature = "testing"))]
pub use self::integration_tests::EventCacheStoreMediaIntegrationTests;
pub use self::{
    media_retention_policy::MediaRetentionPolicy,
    media_service::{EventCacheStoreMedia, IgnoreMediaRetentionPolicy, MediaService},
};
