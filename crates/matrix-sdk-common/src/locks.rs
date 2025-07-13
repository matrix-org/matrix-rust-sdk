// Copyright 2025 The Matrix.org Foundation C.I.C.
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

//! Simplified locks hat panic instead of returning a `Result` when the lock is
//! poisoned.

use std::{
    fmt,
    sync::{Mutex as StdMutex, MutexGuard, RwLock as StdRwLock, RwLockReadGuard, RwLockWriteGuard},
};

use serde::{Deserialize, Serialize};

/// A wrapper around `std::sync::Mutex` that panics on poison.
///
/// This `Mutex` works similarly to the standard library's `Mutex`, except its
/// `lock` method does not return a `Result`. Instead, if the mutex is poisoned,
/// it will panic.
///
/// # Examples
///
/// ```
/// use matrix_sdk_common::locks::Mutex;
///
/// let mutex = Mutex::new(42);
///
/// {
///     let mut guard = mutex.lock();
///     *guard = 100;
/// }
///
/// assert_eq!(*mutex.lock(), 100);
/// ```
#[derive(Default)]
pub struct Mutex<T: ?Sized>(StdMutex<T>);

impl<T> Mutex<T> {
    /// Creates a new `Mutex` wrapping the given value.
    pub const fn new(t: T) -> Self {
        Self(StdMutex::new(t))
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Acquires the lock, panicking if the lock is poisoned.
    ///
    /// This method blocks the current thread until the lock is acquired.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().expect("The Mutex should never be poisoned")
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A wrapper around [`std::sync::RwLock`] that panics on poison.
///
/// This `RwLock` works similarly to the standard library's `RwLock`, except its
/// `read` and `write` methods do not return a `Result`. Instead, if the lock is
/// poisoned, it will panic.
///
/// # Examples
///
/// ```
/// use matrix_sdk_common::locks::RwLock;
///
/// let lock = RwLock::new(42);
///
/// {
///     let read_guard = lock.read();
///     assert_eq!(*read_guard, 42);
/// }
/// {
///     let mut write_guard = lock.write();
///     *write_guard = 100;
/// }
/// assert_eq!(*lock.read(), 100);
/// ```
#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RwLock<T: ?Sized>(StdRwLock<T>);

impl<T> RwLock<T> {
    /// Creates a new `RwLock` wrapping the given value.
    pub const fn new(t: T) -> Self {
        Self(StdRwLock::new(t))
    }
}

impl<T: ?Sized> RwLock<T> {
    /// Acquires a mutable write lock, panicking if the lock is poisoned.
    ///
    /// This method blocks the current thread until the lock is acquired.
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.0.write().expect("The RwLock should never be poisoned")
    }

    /// Acquires a shared read lock, panicking if the lock is poisoned.
    ///
    /// This method blocks the current thread until the lock is acquired.
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.0.read().expect("The RwLock should never be poisoned")
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T> From<T> for RwLock<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}
