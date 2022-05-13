# SDK 0.5 - stores and native crypto

The @matrix-org/rust team is happy to present to the wider public version 0.5 of the [`matrix-rust-sdk`][sdk], now available on crates.io for your convenience. This marks an important milestone after over half a year of work since the latest release (0.4) and with many new team members working on it. It comes with many bug fixes, improvements and updates, but a few we'd like to highlight here:

## Better, safer, native-er crypto

One of the biggest improvements under the hood is the replacement of the former crypto core written in C, libolm, with the fully rustic, fully audited [Vodozemac][vodozemac]. [Vodozemac][vodozemac] is a much more robust new implementation of the olm crypto primitives and engines with all the learning included and pitfalls from the previous C implementation avoided in a freshly baked, very robust library. With this release both matrix-sdk-crypto and matrix-sdk (when the `e2e-encryption` is enabled, which it is by default) use vodozemac to handle all crypto needs. 

## Stores, stores, stores

This release has also seen a major refactoring around the state and crypto store primitives and implementations. From this release forward, the implementation of the storage layer is truly modular and pluggable, with the default implementation (based on `sled`) even being a separate crate. Furthermore this release has also additional support for the [`indexeddb`-storage][indexeddb] layer for in-browser WASM needs. As before, the base still ships with an in-memory store, but if none of them fits your needs, you are very much invited to build your own - we recommend looking at the implementation of the existing [`matrix-sdk-sled`][sled] to understand what is needed.

We've further extended and cleaned up the storage API and hardened the two existing implementations: both `sled` and `indexeddb` will now encrypt _all metadata_, including all keys, if a passphrase is given. Even a core dump of a database won't show any strings or other identifiable data. Both implementation also hold a database version string now, allowing future migrations of the database schema while we move forward.

## WebAssembly 

It already came through in the previous paragraph: we now fully support `wasm` as a primary target for the main sdk and its dependencies (browser for now). While it was already possible to build the SDK for wasm before, if you were lucky and had the specific emscripten setup, even crypto didn't always fail to compile, our move to vodozemac frees us from these problems and our CI now makes sure all our PRs will continue to build on WebAssembly, too. And with the `indexeddb`-store, we also have a fully persistent storage layer implementation for all your in-browser matrix needs built in.

## More features

Of course a lot more has happened over the last few months as we've ramped up our work on the SDK. Most notably, you can now check whether a room has been created as a `space`, and we offer nice automatic thumbnailing and resizing support if you enable `image`. Additionally, there is a very early event-timeline API behind the `experimental-timeline`-feature-flag. Just to name a few. We really recommend you migrate from the older version to this one. 

## Process and Workflow

We've also ramped up our CI, parallelized it a lot, while adding more tests. In general we don't merge anything breaking the tests, thus our `main` can also be considered `stable` to build again, though things might be changed without prior notice. We recommend keeping an eye out in our [team chat][chat] to stay current.

### Versions

This and all future releases of matrix-sdk adhere to [semver][semver] and we do our best to stick to it for the default-feature-set. As you will see only the main crate `matrix-sdk` is actually versioned at `0.5`, while other crates - especially newer ones - have different versions attached. We recommend to sticking to the high-level matrix-sdk-crate wherever reasonable as lower level crates might change more often, also in breaking fashion. 

With this release all our crates are build with `rust v2021` and need a rust compiler of at least `1.60` (due to our need for weak-dependencies).

### Conventional Commit

We also want to make it easier for external parties to follow changes and stay up to date with what's-what, so, with this release, we are implementing the [conventional commits][cc] standard for our commits and will start putting in automatic changelog generation. Which makes it easier for us to create comprehensive changelogs for releases, but also for anyone following `main` to figure out what changed.

[sdk]: https://crates.io/crates/matrix-sdk/
[vodozemac]: https://github.com/matrix-org/vodozemac
[sled]: https://crates.io/crates/matrix-sdk-sled/
[indexeddb]: https://crates.io/crates/matrix-sdk-indexeddb/
[chat]: https://matrix.to/#/#matrix-rust-sdk:matrix.org
[semver]: https://semver.org/
[cc]: http://conventionalcommits.org/ 
