Ensure any `SaveLockedStateStore` functions which may interfere with its
implementation of `StateStore::save_changes` are synchronized using the
underlying lock.
