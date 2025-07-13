// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::{mem::ManuallyDrop, ops::Deref};

use ruma::{MilliSecondsSinceUnixEpoch, UInt};
use tracing::warn;

use crate::runtime::get_runtime_handle;

#[derive(Debug, Clone)]
pub struct Timestamp(u64);

impl From<MilliSecondsSinceUnixEpoch> for Timestamp {
    fn from(date: MilliSecondsSinceUnixEpoch) -> Self {
        Self(date.0.into())
    }
}

uniffi::custom_newtype!(Timestamp, u64);

pub(crate) fn u64_to_uint(u: u64) -> UInt {
    UInt::new(u).unwrap_or_else(|| {
        warn!("u64 -> UInt conversion overflowed, falling back to UInt::MAX");
        UInt::MAX
    })
}

/// Tiny wrappers for data types that must be dropped in the context of an async
/// runtime.
///
/// This is useful whenever such a data type may transitively call some
/// runtime's `block_on` function in their `Drop` impl (since we lack async drop
/// at the moment), like done in some `deadpool` drop impls.
pub(crate) struct AsyncRuntimeDropped<T>(ManuallyDrop<T>);

impl<T> AsyncRuntimeDropped<T> {
    /// Create a new wrapper for this type that will be dropped under an async
    /// runtime.
    pub fn new(val: T) -> Self {
        Self(ManuallyDrop::new(val))
    }
}

impl<T> Drop for AsyncRuntimeDropped<T> {
    fn drop(&mut self) {
        let _guard = get_runtime_handle().enter();
        // SAFETY: self.inner is never used again, which is the only requirement
        //         for ManuallyDrop::drop to be used safely.
        unsafe {
            ManuallyDrop::drop(&mut self.0);
        }
    }
}

// What is an `AsyncRuntimeDropped<T>`, if not a `T` in disguise?
impl<T> Deref for AsyncRuntimeDropped<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Clone> Clone for AsyncRuntimeDropped<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
