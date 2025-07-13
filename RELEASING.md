# Releasing and publishing the SDK

While the release process can be handled manually, `cargo-release` has been
configured to make it more convenient.

By default, [`cargo-release`](https://github.com/crate-ci/cargo-release) assumes
that no pull request is required to cut a release. However, since the SDK
repo is set up so that each push requires a pull request, we need to slightly
deviate from the default workflow. A `cargo-xtask` has been created to make the
process as smooth as possible.

The procedure is as follows:

1. Switch to a release branch:

   ```bash
   git switch -c release-x.y.z
   ```

2. Prepare the release. This will update the `README.md`, set the versions in
   the `CHANGELOG.md` file, and bump the version in the `Cargo.toml` file.

   ```bash
   cargo xtask release prepare --execute minor|patch|rc
   ```

3. Double-check and edit the `CHANGELOG.md` and `README.md` if necessary. Once you are
   satisfied, push the branch and open a PR.

   ```bash
   git push --set-upstream origin/release-x.y.z
   ```

4. Pass the review and merge the branch as you would with any other branch.

5. Create tags for your new release, publish the release on crates.io and push
   the tags:

   ```bash
   # Switch to main first.
   git switch main
   # Pull in the now-merged release commit(s).
   git pull
   # Create tags, publish the release on crates.io, and push the tags.
   cargo xtask release publish --execute
   ```

   For more information on cargo-release: https://github.com/crate-ci/cargo-release
