This crate implements the base to build a [Matrix](https://matrix.org/) client
library.

## Crate Feature Flags

The following crate feature flags are available:

* `encryption`: Enables end-to-end encryption support in the library.
* `sled_cryptostore`: Enables a Sled based store for the encryption
  keys. If this is disabled and `encryption` support is enabled the keys will
  by default be stored only in memory and thus lost after the client is
  destroyed.
* `sled_state_store`: Enables a Sled based store for the storage of the local
  client state.
