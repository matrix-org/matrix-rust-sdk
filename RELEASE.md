# Releasing `matrix-rust-sdk`

- Make sure to bump all the crates to *the same version number*, and commit that (along with the
  changes to the `Cargo.lock` file).
- Create a `git tag` for the current version, following the format `major.minor.patch`, e.g. `0.7.0`.
- Push the tag: `git push origin 0.7.0`
- Publish all the crates, in topological order of the dependency tree:

```
`cargo publish -p matrix-sdk-test-macros`
`cargo publish -p matrix-sdk-test`
`cargo publish -p matrix-sdk-common`
`cargo publish -p matrix-sdk-qrcode`
`cargo publish -p matrix-sdk-store-encryption`
`cargo publish -p matrix-sdk-crypto`
`cargo publish -p matrix-sdk-base`
`cargo publish -p matrix-sdk-sqlite`
`cargo publish -p matrix-sdk-indexeddb`
`cargo publish -p matrix-sdk`
`cargo publish -p matrix-sdk-ui`
```
