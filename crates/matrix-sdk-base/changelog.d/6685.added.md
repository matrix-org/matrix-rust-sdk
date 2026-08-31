Add support for storing global user profiles received through the sliding
sync profiles extension ([MSC4262](https://github.com/matrix-org/matrix-spec-proposals/pull/4262)).
Profile updates are carried on the new `StateChanges::global_profiles` field
so they are persisted in the same transaction as the rest of a sync, and can
be read back with `StateStore::get_global_profile`.
