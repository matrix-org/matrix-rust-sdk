Steps for releasing `matrix-sdk-crypto-js`
==========================================

1. Create a new branch, named `release-matrix-sdk-crypto-js-<version>`.
2. Update `CHANGELOG.md`, if necessary.
3. Run `yarn version` to bump the version number and create a tag.
4. Push the branch, but not yet the tag.
5. Create a PR to approve the changes.
6. Once approved:
   1. Update the git tag to the new head of the branch, if necessary.
   2. Push the git tag; doing so triggers the github actions workflow which
      builds and publishes to npm, and creates a draft GH release.
   3. Merge the PR.
7. Update the release on github and publish.
