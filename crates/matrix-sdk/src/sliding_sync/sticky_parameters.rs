//! Sticky parameters are a way to spare bandwidth on the network, by sending
//! request parameters once and have the server remember them.
//!
//! The set of sticky parameters have to be agreed upon by the server and the
//! client; this is defined in the
//! [MSC](https://github.com/matrix-org/matrix-spec-proposals/blob/kegan/sync-v3/proposals/3575-sync.md).

use ruma::{OwnedTransactionId, TransactionId};

/// An `OwnedTransactionId` that is either initialized at creation, or
/// lazily-generated once.
#[derive(Debug)]
pub struct LazyTransactionId {
    txn_id: Option<OwnedTransactionId>,
}

impl LazyTransactionId {
    /// Create a new `LazyTransactionId`, not set.
    pub fn new() -> Self {
        Self { txn_id: None }
    }

    /// Get (or create it, if never set) a `TransactionId`.
    pub fn get_or_create(&mut self) -> &TransactionId {
        self.txn_id.get_or_insert_with(TransactionId::new)
    }

    /// Attempt to get the underlying `TransactionId` without creating it, if
    /// missing.
    pub fn get(&self) -> Option<&TransactionId> {
        self.txn_id.as_deref()
    }
}

#[cfg(test)]
impl LazyTransactionId {
    /// Create a `LazyTransactionId` for a given known transaction id. For
    /// testing only.
    pub fn from_owned(owned: OwnedTransactionId) -> Self {
        Self { txn_id: Some(owned) }
    }
}

/// A trait to implement for data that can be sticky, given a context.
pub trait StickyData {
    /// Request type that will be applied to, if the sticky parameters have been
    /// invalidated before.
    type Request;

    /// Apply the current data onto the request.
    fn apply(&self, request: &mut Self::Request);
}

/// Helper data structure to manage sticky parameters, for any kind of data.
///
/// Initially, the provided data is considered to be invalidated, so it's
/// applied onto the request the first time it's sent. Any changes to the
/// wrapped data happen via `[Self::data_mut]`, which invalidates the sticky
/// parameters; they will be applied automatically to the next request.
///
/// When applying sticky parameters, we will also remember the transaction id
/// that was generated for us, stash it, so we can match the response against
/// the transaction id later, and only consider the data isn't invalidated
/// anymore (we say it's "committed" in that case) if the response's transaction
/// id match what we expect.
#[derive(Debug)]
pub struct SlidingSyncStickyManager<D: StickyData> {
    /// The data managed by this sticky manager.
    data: D,

    /// Was any of the parameters invalidated? If yes, reinitialize them.
    invalidated: bool,

    /// If the sticky parameters were applied to a given request, this is
    /// the transaction id generated for that request, that must be matched
    /// upon in the next call to `commit()`.
    txn_id: Option<OwnedTransactionId>,
}

impl<D: StickyData> SlidingSyncStickyManager<D> {
    /// Create a new `StickyManager` for the given data.
    ///
    /// Always assume the initial data invalidates the request, at first.
    pub fn new(data: D) -> Self {
        Self { data, txn_id: None, invalidated: true }
    }

    /// Get a mutable reference to the managed data.
    ///
    /// Will invalidate the sticky set by default. If you don't need to modify
    /// the data, use `Self::data()`; if you're not sure you're going to modify
    /// the data, it's best to first use `Self::data()` then `Self::data_mut()`
    /// when you're sure.
    pub fn data_mut(&mut self) -> &mut D {
        self.invalidated = true;
        &mut self.data
    }

    /// Returns a non-invalidating reference to the managed data.
    pub fn data(&self) -> &D {
        &self.data
    }

    /// May apply some the managed sticky parameters to the given request.
    ///
    /// After receiving the response from this sliding sync, the caller MUST
    /// also call [`Self::maybe_commit`] with the transaction id from the
    /// server's response.
    ///
    /// If no `txn_id` is provided, it will generate one that can be reused
    /// later.
    pub fn maybe_apply(&mut self, req: &mut D::Request, txn_id: &mut LazyTransactionId) {
        if self.invalidated {
            let txn_id = txn_id.get_or_create();
            self.txn_id = Some(txn_id.to_owned());
            self.data.apply(req);
        }
    }

    /// May mark the managed data as not invalidated anymore, if the transaction
    /// id received from the response matches the one received from the request.
    pub fn maybe_commit(&mut self, txn_id: &TransactionId) {
        if self.invalidated && self.txn_id.as_deref() == Some(txn_id) {
            self.invalidated = false;
        }
    }

    #[cfg(test)]
    pub fn is_invalidated(&self) -> bool {
        self.invalidated
    }
}

#[cfg(test)]
mod tests {
    use super::{LazyTransactionId, SlidingSyncStickyManager, StickyData};

    struct EmptyStickyData;

    impl StickyData for EmptyStickyData {
        type Request = bool;

        fn apply(&self, req: &mut Self::Request) {
            // Mark that applied has had an effect.
            *req = true;
        }
    }

    #[test]
    fn test_sticky_parameters_api_non_invalidated_no_effect() {
        let mut sticky = SlidingSyncStickyManager::new(EmptyStickyData);

        // At first, it's always invalidated.
        assert!(sticky.is_invalidated());

        let mut applied = false;
        let mut txn_id = LazyTransactionId::new();
        sticky.maybe_apply(&mut applied, &mut txn_id);
        assert!(applied);
        assert!(sticky.is_invalidated());
        assert!(txn_id.get().is_some(), "a transaction id was lazily generated");

        // Committing with the wrong transaction id won't commit.
        sticky.maybe_commit("tid456".into());
        assert!(sticky.is_invalidated());

        // Providing the correct transaction id will commit.
        sticky.maybe_commit(txn_id.get().unwrap());
        assert!(!sticky.is_invalidated());

        // Applying without being invalidated won't do anything, and not generate a
        // transaction id.
        let mut txn_id = LazyTransactionId::new();
        let mut applied = false;
        sticky.maybe_apply(&mut applied, &mut txn_id);

        assert!(!applied);
        assert!(!sticky.is_invalidated());
        assert!(txn_id.get().is_none());
    }
}
