Ensure that `IndexeddbEventCacheStore` properly pushes and removes events from a chunk.
Prior to these changes, pushing an event could erroneously replace an existing event, but
now it only replaces an existing event if it is being promoted from out-of-band to in-band.
Additionally, removing an event now properly shifts the indices of subsequent events in
the chunk.
