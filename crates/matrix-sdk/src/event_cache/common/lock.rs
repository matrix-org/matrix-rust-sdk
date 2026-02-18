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

//! A read-write, cross-thread, and cross-process lock.
//!
//! Such a lock is usually used to manage states of the various caches in the
//! Event Cache, e.g. [`RoomEventCache`].
//!
//! [`RoomEventCache`]: super::super::RoomEventCache

use matrix_sdk_base::event_cache::store::{
    EventCacheStoreLock, EventCacheStoreLockGuard, EventCacheStoreLockState,
};
use tokio::sync::{Mutex, RwLock, RwLockWriteGuard};

use super::super::Result;

/// State for a single cache.
///
/// This aims at containing all the inner mutable states that ought to be
/// updated at the same time.
pub struct StateLock<S> {
    /// The per-thread lock around the real state.
    locked_state: RwLock<S>,

    /// A lock taken to avoid multiple attempts to upgrade from a read lock
    /// to a write lock.
    ///
    /// Please see inline comment of [`Self::read`] to understand why it
    /// exists.
    state_lock_upgrade_mutex: Mutex<()>,
}

impl<S> StateLock<S> {
    /// Construct a new [`StateLock`].
    pub fn new_inner(state: S) -> Self {
        Self { locked_state: RwLock::new(state), state_lock_upgrade_mutex: Mutex::new(()) }
    }

    /// Lock this [`StateLock`] with per-thread shared access.
    ///
    /// This method locks the per-thread lock over the state, and then locks
    /// the cross-process lock over the store. It returns an RAII guard
    /// which will drop the read access to the state and to the store when
    /// dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reset to the last chunk.
    pub async fn read<'a>(&'a self) -> Result<StateLockReadGuard<'a, S>>
    where
        S: Store,
        StateLockWriteGuard<'a, S>: Reload,
    {
        // Only one call at a time to `read` is allowed.
        //
        // Why? Because in case the cross-process lock over the store is dirty, we need
        // to upgrade the read lock over the state to a write lock.
        //
        // ## Upgradable read lock
        //
        // One may argue that this upgrades can be done with an _upgradable read lock_
        // [^1] [^2]. We don't want to use this solution: an upgradable read lock is
        // basically a mutex because we are losing the shared access property, i.e.
        // having multiple read locks at the same time. This is an important property to
        // hold for performance concerns.
        //
        // ## Downgradable write lock
        //
        // One may also argue we could first obtain a write lock over the state from the
        // beginning, thus removing the need to upgrade the read lock to a write lock.
        // The write lock is then downgraded to a read lock once the dirty is cleaned
        // up. It can potentially create a deadlock in the following situation:
        //
        // - `read` is called once, it takes a write lock, then downgrades it to a read
        //   lock: the guard is kept alive somewhere,
        // - `read` is called again, and waits to obtain the write lock, which is
        //   impossible as long as the guard from the previous call is not dropped.
        //
        // ## “Atomic” read and write
        //
        // One may finally argue to first obtain a read lock over the state, then drop
        // it if the cross-process lock over the store is dirty, and immediately obtain
        // a write lock (which can later be downgraded to a read lock). The problem is
        // that this write lock is async: anything can happen between the drop and the
        // new lock acquisition, and it's not possible to pause the runtime in the
        // meantime.
        //
        // ## Semaphore with 1 permit, aka a Mutex
        //
        // The chosen idea is to allow only one execution at a time of this method: it
        // becomes a critical section. That way we are free to “upgrade” the read lock
        // by dropping it and obtaining a new write lock. All callers to this method are
        // waiting, so nothing can happen in the meantime.
        //
        // Note that it doesn't conflict with the `write` method because this latter
        // immediately obtains a write lock, which avoids any conflict with this method.
        //
        // [^1]: https://docs.rs/lock_api/0.4.14/lock_api/struct.RwLock.html#method.upgradable_read
        // [^2]: https://docs.rs/async-lock/3.4.1/async_lock/struct.RwLock.html#method.upgradable_read
        let _state_lock_upgrade_guard = self.state_lock_upgrade_mutex.lock().await;

        // Obtain a read lock.
        let state_guard = self.locked_state.read().await;

        match state_guard.store().lock().await? {
            EventCacheStoreLockState::Clean(store_guard) => {
                Ok(StateLockReadGuard { state: state_guard, store: store_guard })
            }
            EventCacheStoreLockState::Dirty(store_guard) => {
                // Drop the read lock, and take a write lock to modify the state.
                // This is safe because only one reader at a time (see
                // `Self::state_lock_upgrade_mutex`) is allowed.
                drop(state_guard);
                let state_guard = self.locked_state.write().await;

                let mut guard = StateLockWriteGuard { state: state_guard, store: store_guard };

                // Reload the state.
                guard.reload().await?;

                // All good now, mark the cross-process lock as non-dirty.
                EventCacheStoreLockGuard::clear_dirty(&guard.store);

                // Downgrade the write guard to a read guard.
                let guard = guard.downgrade();

                Ok(guard)
            }
        }
    }

    /// Lock this [`StateLock`] with exclusive per-thread write access.
    ///
    /// This method locks the per-thread lock over the state, and then locks
    /// the cross-process lock over the store. It returns an RAII guard
    /// which will drop the write access to the state and to the store when
    /// dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reset to the last chunk.
    pub async fn write<'a>(&'a self) -> Result<StateLockWriteGuard<'a, S>>
    where
        S: Store,
        StateLockWriteGuard<'a, S>: Reload,
    {
        let state_guard = self.locked_state.write().await;

        match state_guard.store().lock().await? {
            EventCacheStoreLockState::Clean(store_guard) => {
                Ok(StateLockWriteGuard { state: state_guard, store: store_guard })
            }
            EventCacheStoreLockState::Dirty(store_guard) => {
                let mut guard = StateLockWriteGuard { state: state_guard, store: store_guard };

                // Reload the state.
                guard.reload().await?;

                // All good now, mark the cross-process lock as non-dirty.
                EventCacheStoreLockGuard::clear_dirty(&guard.store);

                Ok(guard)
            }
        }
    }
}

/// The read lock guard returned by [`StateLock::read`].
pub struct StateLockReadGuard<'a, S> {
    /// The per-thread read lock guard over the state `S`.
    pub state: tokio::sync::RwLockReadGuard<'a, S>,

    /// The cross-process lock guard over the store.
    pub store: EventCacheStoreLockGuard,
}

/// The write lock guard return by [`StateLock::write`].
pub struct StateLockWriteGuard<'a, S> {
    /// The per-thread write lock guard over the state `S`.
    pub state: RwLockWriteGuard<'a, S>,

    /// The cross-process lock guard over the store.
    pub store: EventCacheStoreLockGuard,
}

impl<'a, S> StateLockWriteGuard<'a, S> {
    /// Synchronously downgrades a write lock into a read lock.
    ///
    /// The per-thread/state lock is downgraded atomically, without allowing
    /// any writers to take exclusive access of the lock in the meantime.
    ///
    /// It returns an RAII guard which will drop the read access to the
    /// state and to the store when dropped.
    fn downgrade(self) -> StateLockReadGuard<'a, S> {
        StateLockReadGuard { state: self.state.downgrade(), store: self.store }
    }
}

/// Trait to give access to the [`EventCacheStoreLock`].
pub trait Store {
    /// Return a reference to [`EventCacheStoreLock`].
    fn store(&self) -> &EventCacheStoreLock;
}

/// Trait to reload the state `S` in [`StateLock`].
pub trait Reload {
    /// Reload the state entirely from zero.
    async fn reload(&mut self) -> Result<()>;
}
