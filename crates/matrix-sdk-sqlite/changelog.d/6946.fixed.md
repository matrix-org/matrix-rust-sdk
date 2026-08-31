Fix a bug in `SqliteCryptoStore::mark_inbound_group_sessions_as_backed_up`,
`SqliteStateStore::get_profiles` and `SqliteStateStore::get_global_profiles`,
where passing a large number of sessions or users were erroring to “too many SQL
variables” in SQLite. This error is due to a manual misuse of the
`repeat_vars` function in the `chunk_large_query_over` method. The fix is to
make misuses impossible by introducing the new `ChunkFromLargeQuery` type,
replacing the `Vec<Key>` in `chunk_large_query_over`, removing the need to
manually use `repeat_vars`.
