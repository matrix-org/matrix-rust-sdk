# Changelog

All notable changes to this project will be documented in this file.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Bug Fixes

- Remove the usage of an unwrap in the `StoreCipher::import_with_key` method.
  This could have lead to panics if the second argument was an invalid
  `StoreCipher` export.

## [0.9.0] - 2024-12-18

No notable changes in this release.

## [0.8.0] - 2024-11-19

No notable changes in this release.
