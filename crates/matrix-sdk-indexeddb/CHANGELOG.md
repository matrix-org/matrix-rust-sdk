# UNRELEASED

- Improve the efficiency of objects stored in the crypto store. ([#3645](https://github.com/matrix-org/matrix-rust-sdk/pull/3645))

- Add new method `IndexeddbCryptoStore::open_with_key`. ([#3423](https://github.com/matrix-org/matrix-rust-sdk/pull/3423))

- `save_change` performance improvement, all encryption and serialization
  is done now outside of the db transaction.
