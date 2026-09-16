# Changelog

All notable changes to this project will be documented in this file.

<!-- changelog start -->

## [0.19.0](https://github.com/matrix-org/matrix-rust-sdk/tree/0.19.0) - 2026-09-16

### Changed

- Remove existing gossip requests when a new request for the same secret is
  made. ([#6631](https://github.com/matrix-org/matrix-rust-sdk/pulls/6631))
- Use `Readonly` rather than `Readwrite` transactions in
  `EventCacheStore::{load_all_chunks, load_all_chunks_metadata}` as these
  functions only read data from the database.
  ([#6788](https://github.com/matrix-org/matrix-rust-sdk/pulls/6788))
- Defer `await`s in write operations in `Transaction` until calling
  `Transaction::commit`. Additionally, remove `async` modifier from functions
  that no longer need to be asynchronous as a result of the change.
  ([#6892](https://github.com/matrix-org/matrix-rust-sdk/pulls/6892))
- Load last chunk and max chunk id concurrently in
  `IndexeddbEventCacheStore::load_last_chunk`.
  ([#7028](https://github.com/matrix-org/matrix-rust-sdk/pulls/7028))

### Fixed

- Ensure that `IndexeddbEventCacheStore` properly pushes and removes events from
  a chunk. Prior to these changes, pushing an event could erroneously replace an
  existing event, but now it only replaces an existing event if it is being
  promoted from out-of-band to in-band. Additionally, removing an event now
  properly shifts the indices of subsequent events in the chunk.
  ([#6782](https://github.com/matrix-org/matrix-rust-sdk/pulls/6782))
- Fix large attachment uploads failing with `Invalid array length` on WASM.
  IndexedDB media content is now serialized as a compact `Uint8Array` instead of
  a JavaScript `Array` containing one element per byte.
  ([#6826](https://github.com/matrix-org/matrix-rust-sdk/pulls/6826))
- Ensure that every instance of an `Event` across all `LinkedChunk`s is updated
  when one instance of that `Event` is updated. For efficiency, a new index was
  added to the `EVENTS` object store that tracks the `EventId` of an `Event`, so
  that all instances of an `Event` across all `LinkedChunk`s could be retrieved
  with a single query.
  ([#6872](https://github.com/matrix-org/matrix-rust-sdk/pulls/6872))

## [0.18.0](https://github.com/matrix-org/matrix-rust-sdk/tree/0.18.0) - 2026-06-02

No significant changes.

## [0.17.0] - 2026-05-08

### Features

- Add support in the implementation of `EventCacheStore` for
  having duplicate events in a room, where each duplicate is in a different
  `LinkedChunk`. This is useful, e.g., when an event is in a room and a
  thread in that room. The change involves a database migration where
  the `EVENTS` object store is cleared and then modified so that the
  `ROOM` index no longer requires keys to be unique.
  ([#6200](https://github.com/matrix-org/matrix-rust-sdk/pull/6200))
- Implement `CryptoStore::get_pending_key_bundle_details_for_room` and
  `CryptoStore::get_all_rooms_pending_key_bundle`, and process
  `rooms_pending_key_bundle` field in `Changes`.
  ([#6199](https://github.com/matrix-org/matrix-rust-sdk/pull/6199)),
  ([#6233](https://github.com/matrix-org/matrix-rust-sdk/pull/6233))
- Expose implementations of `EventCacheStore` and `MediaStore` and add a
  composite type for initializing all stores with a single function - i.e.,
  `IndexeddbStores::open`. Additionally, allow feature flags for each of the
  stores to be used independent of and in combination with the others.
  ([#5946](https://github.com/matrix-org/matrix-rust-sdk/pull/5946))
- Implement new method `CyptoStore::has_downloaded_all_room_keys`, and process
  `room_key_backups_fully_downloaded` field in `Changes`.
  ([#6017](https://github.com/matrix-org/matrix-rust-sdk/pull/6017))
  ([#6044](https://github.com/matrix-org/matrix-rust-sdk/pull/6044))
- [**breaking**] In `EventCacheStore::handle_linked_chunk_updates`, new chunks
  may no longer reference chunk identifiers which do not yet exist in the store
  ([#6061](https://github.com/matrix-org/matrix-rust-sdk/pull/6061))

### Bug fixes

- Ensure that encrypted tests are run with a `StoreCipher`. This happened to
  reveal tests which fail in an encrypted `EventCacheStore`, which required
  fixing queries for all events in a room.
  ([#5933](https://github.com/matrix-org/matrix-rust-sdk/pull/5933))

### Refactor

- Add migration to `IndexeddbCryptoStore` that removes cross-process lock
  generation key from `CORE` object store, as this is tracked in `LEASE_LOCKS`
  object store.
  ([#6326](https://github.com/matrix-org/matrix-rust-sdk/pull/6326))

## [0.16.1] - 2026-05-08

No notable changes in this release.

## [0.16.0] - 2025-12-04

### Features

- Implement new method `CryptoStore::get_withheld_sessions_by_room_id`.
  ([#5819](https://github.com/matrix-org/matrix-rust-sdk/pull/5819))
- [**breaking**] `IndexeddbCryptoStore::get_withheld_info` now returns
  `Result<Option<RoomKeyWithheldEntry>, ...>`.
  ([#5737](https://github.com/matrix-org/matrix-rust-sdk/pull/5737))
- Implement `StateStore::upsert_thread_subscriptions()` method for bulk upserts.
  ([#5848](https://github.com/matrix-org/matrix-rust-sdk/pull/5848))

### Performance

- Improve performance of certain media queries in `MediaStore` implementation by
  storing media content and media metadata in separate object stores in
  IndexedDB (see
  [#5795](https://github.com/matrix-org/matrix-rust-sdk/pull/5795)).

## [0.14.0] - 2025-09-04

No notable changes in this release.

## [0.13.0] - 2025-07-10

### Features

- Add support for received room key bundle data, as required by encrypted
  history sharing
  ((MSC4268)[[https://github.com/matrix-org/matrix-spec-proposals/pull/4268][https-github-com-matrix-org-matrix-spec-proposals-pull-4268])).
  ([#5276](https://github.com/matrix-org/matrix-rust-sdk/pull/5276))

## [0.12.0] - 2025-06-10

No notable changes in this release.

## [0.11.0] - 2025-04-11

No notable changes in this release.

## [0.10.0] - 2025-02-04

No notable changes in this release.

## [0.9.0] - 2024-12-18

No notable changes in this release.

## [0.8.0] - 2024-11-19

### Features

- Improve the efficiency of objects stored in the crypto store.
  ([#3645](https://github.com/matrix-org/matrix-rust-sdk/pull/3645),
  [#3651](https://github.com/matrix-org/matrix-rust-sdk/pull/3651))

- Add new method `IndexeddbCryptoStore::open_with_key`.
  ([#3423](https://github.com/matrix-org/matrix-rust-sdk/pull/3423))

- `save_change` performance improvement, all encryption and serialization
  is done now outside of the db transaction.

### Bug fixes

- Use the `DisplayName` struct to protect against homoglyph attacks.

[https-github-com-matrix-org-matrix-spec-proposals-pull-4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
