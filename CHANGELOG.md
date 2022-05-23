# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
### Added
- A `CryptoStore` implementation
- Added a convenience `store_config` method, that returns a preconfigured store config

### Changes
- Improved performance by reducing the amount of allocations and copy operations done
- Changed the places trait bounds are included

### Removed
- Many internal functions of the StateStore are no longer public.

### Fixes
- Fixes a type issue when inserting members in postgresql
- Makes `remove_room` work with postgresql (it does not support prepared queries with multiple statements)

## [0.1.0-beta.1] - 2022-05-12

Initial version of `matrix-sdk-statestore-sql`. Includes an implementation of the `StateStore` trait.

[Unreleased]: https://github.com/DarkKirb/matrix-sdk-statestore-sql/compare/v0.1.0-beta.1...HEAD
[0.1.0-beta.1]: https://github.com/DarkKirb/matrix-sdk-statestore-sql/releases/tag/v0.1.0-beta.1
