## [0.19.0](https://github.com/matrix-org/matrix-rust-sdk/tree/0.19.0) - 2026-09-16

### Added

- `MediaScanResponse` is now exposed in the FFI layer too, and there is a new
  `ContentScannerMediaFetcher::with_content_scanner(Arc<ContentScanner>)` method
  that allows you to create a media fetcher that will reuse the existing
  `ContentScanner` instance instead of creating a new one.
  ([#6689](https://github.com/matrix-org/matrix-rust-sdk/pull/6689))
- Add `ContentScanner` and `ContentScannerMediaFetcher` to be able to scan and
  download media files using a content scanner server.
  ([#6625](https://github.com/matrix-org/matrix-rust-sdk/pull/6625))

### Fixed

- Add media content decryption to the `ContentScannerMediaFetcher` when the
  media source provided is `MediaSource::Encrypted`.
  ([#6779](https://github.com/matrix-org/matrix-rust-sdk/pull/6779))
