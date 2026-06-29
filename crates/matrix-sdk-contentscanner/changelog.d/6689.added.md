`MediaScanResponse` is now exposed in the FFI layer too, and there is a new
`ContentScannerMediaFetcher::with_content_scanner(Arc<ContentScanner>)` method that allows you to create a media fetcher
that will reuse an existing `ContentScanner` instance instead of creating a new one.