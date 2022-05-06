This crate implements the base to build a [Matrix](https://matrix.org/) client
library.

## Crate Feature Flags

The following crate feature flags are available:

* `encryption`: Enables end-to-end encryption support in the library.
* `qrcode`: Enables QRcode generation and reading code.
* `testing`: Provides facilities and functions for tests, in particular for integration testing store implementations. ATTENTION: do not ever use outside of tests, we do not provide any stability warantees on these, these are merely helpers. If you find you _need_ any function provided here outside of tests, please open a Github Issue and inform us about your use case for us to consider.
