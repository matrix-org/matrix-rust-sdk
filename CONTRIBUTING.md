# Contributing to matrix-rust-sdk

## Chat rooms

In addition to our [main Matrix room], we have a development room at
[#matrix-rust-sdk-dev:flipdot.org]. Please use it to discuss potential changes,
the overall direction of development and related topics.

[main Matrix room]: https://matrix.to/#/#matrix-rust-sdk:matrix.org
[#matrix-rust-sdk-dev:flipdot.org]: https://matrix.to/#/#matrix-rust-sdk-dev:flipdot.org

## Testing

You can run most of the tests that also happen in CI also using
`cargo xtask ci`. This needs a few dependencies to be installed, as it also runs
automatic WASM tests:

```bash
rustup component add clippy
cargo install cargo-nextest typos-cli wasm-pack
```

If you want to execute only one part of CI, there are a few sub-commands (see
`cargo xtask ci --help`).

Some tests are not automatically run in `cargo xtask ci`, for example the
integration tests that need a running synapse instance. These tests reside in
`./testing/matrix-sdk-integration-testing`. See its
[README](./testing/matrix-sdk-integration-testing/README.md) to easily set up a
synapse for testing purposes.


## Pull requests

Ideally, a PR should have a *proper title*, with *atomic logical commits*, and
each commit should have a *good commit message*.

A *proper PR title* would be a one-liner summary of the changes in the PR,
following the same guidelines of a good commit message, including the
area/feature prefix. Something like `FFI: Allow logs files to be pruned.` would
be a good PR title.

(An additional bad example of a bad PR title would be `mynickname/branch name`,
that is, just the branch name.)

# Writing changelog entries

We aim to maintain clear and informative changelogs that accurately reflect the
changes in our project. This guide will help you write useful changelog entries
using git-cliff, which fetches changelog entries from commit messages.

## Commit message format

Commit messages should be formatted as Conventional Commits. In addition, some
git trailers are supported and have special meaning (see below).

### Conventional commits

Conventional Commits are structured as follows:

```
<type>(<scope>): <short summary>
```

The type of changes which will be included in changelogs is one of the following:

* `feat`: A new feature
* `fix`: A bug fix
* `doc`: Documentation changes
* `refactor`: Code refactoring
* `perf`: Performance improvements
* `ci`: Changes to CI configuration files and scripts

The scope is optional and can specify the area of the codebase affected (e.g.,
olm, cipher).

### Changelog trailer

In addition to the Conventional Commit format, you can use the `Changelog` git
trailer to specify the changelog message explicitly. When that trailer is
present, its value will be used as the changelog entry instead of the commit's
leading line. The `Breaking-Change` git trailer can be used in a similar manner
if the changelog entry should be marked as a breaking change.


#### Example commit message

```
feat: Add a method to encode Ed25519 public keys to Base64

This patch adds the `Ed25519PublicKey::to_base64()` method, which allows us to
stringify Ed25519 and thus present them to users. It's also commonly used when
Ed25519 keys need to be inserted into JSON.

Changelog: Add the `Ed25519PublicKey::to_base64()` method which can be used to
  stringify the Ed25519 public key.
```

In this commit message, the content specified in the `Changelog` trailer will be
used for the changelog entry.

Be careful to add at least one whitespace after new lines to create a paragraph.

### Security fixes

Commits addressing security vulnerabilities must include specific trailers for
vulnerability metadata. These commits are required to include at least the
`Security-Impact` trailer to indicate that the commit is a security fix.

Security issues have some additional git-trailers:

* `Security-Impact`: The magnitude of harm that can be expected, i.e. low/moderate/high/critical.
* `CVE`: The CVE that was assigned to this issue.
* `GitHub-Advisory`: The GitHub advisory identifier.

Example:

```
fix(crypto): Use a constant-time Base64 encoder for secret key material

This patch fixes a security issue around a side-channel vulnerability[1]
when decoding secret key material using Base64.

In some circumstances an attacker can obtain information about secret
secret key material via a controlled-channel and side-channel attack.

This patch avoids the side-channel by switching to the base64ct crate
for the encoding, and more importantly, the decoding of secret key
material.

Security-Impact: Low
CVE: CVE-2024-40640
GitHub-Advisory: GHSA-j8cm-g7r6-hfpq

Changelog: Use a constant-time Base64 encoder for secret key material
to mitigate side-channel attacks leaking secret key material.
```

## Review process

To streamline the review process and make it easier for maintainers to review
your contributions, follow these basic rules:

1. Do not force push after a review has started. This helps maintainers track
   incremental changes without confusion and makes it easier to follow the
   evolution of the code.

2. Do not mix moves and refactoring with functional changes. Keep these in
   separate commits for clarity. This ensures that the purpose of each commit is
   clear and easy to review.

3. Each commit must compile. If commits don’t compile, git bisect becomes
   unusable, which hampers the debugging process and makes it harder to identify
   the source of issues.

4. Commits should only introduce test failures if they are proving that a bug
   exists. New features should never introduce test failures. Test failures
   should only be used to demonstrate existing bugs, not as part of adding new
   functionality.

5. Keep PRs on topic and small. Large PRs are harder to review and more prone to
   delays. Create small, focused commits that address a single topic. Use a
   combination of [git add] -p or git checkout -p to split changes into logical
   units. This makes your work easier to review and reduces the chance of
   introducing unrelated changes.

[git add]: https://git-scm.com/docs/git-add#Documentation/git-add.txt---patch
[git checkout]: https://git-scm.com/docs/git-checkout#Documentation/git-checkout.txt---patch

### Addressing review comments using fixup commits

So you posted a PR and the maintainers aren't quite happy with it. Here are some
guidelines to make the maintainers life easier and increase the chances that
your PR will be reviewed swiftly.

1. Use [fixup] commits. When addressing reviewer feedback, you can create fixup
commits. These commits mark your changes as corrections of specific previous
commits in the PR.

Example:

```bash
git commit --fixup=<commit-hash>
```

This command creates a new commit that refers to an existing one, making it
easier to rebase and squash later while showing reviewers the history of fixes.
For extra points, link to the fixup commit in the thread where the change was
requested.

2. After all requested changes were addressed, feel free to re-request a review.
   People might not notice that all changes were addressed.

3. Once the PR has been approved, rebase your PR to squash all the fixup
   commits, the [autosquash] option can help with this.

```bash
git rebase main --interactive --autosquash
```

[fixup]: https://git-scm.com/docs/git-commit#Documentation/git-commit.txt---fixupamendrewordltcommitgt
[autosquash]: https://git-scm.com/docs/git-rebase#Documentation/git-rebase.txt---autosquash

## Sign off

In order to have a concrete record that your contribution is intentional
and you agree to license it under the same terms as the project's license, we've
adopted the same lightweight approach that the [Linux Kernel](https://www.kernel.org/doc/Documentation/SubmittingPatches),
[Docker](https://github.com/docker/docker/blob/master/CONTRIBUTING.md), and many other
projects use: the DCO ([Developer Certificate of Origin](http://developercertificate.org/)).
This is a simple declaration that you wrote the contribution or otherwise have the right
to contribute it to Matrix:

```
Developer Certificate of Origin
Version 1.1

Copyright (C) 2004, 2006 The Linux Foundation and its contributors.
660 York Street, Suite 102,
San Francisco, CA 94110 USA

Everyone is permitted to copy and distribute verbatim copies of this
license document, but changing it is not allowed.

Developer's Certificate of Origin 1.1

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same open source license (unless I am
    permitted to submit under a different license), as indicated
    in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including all
    personal information I submit with it, including my sign-off) is
    maintained indefinitely and may be redistributed consistent with
    this project or the open source license(s) involved.
```

If you agree to this for your contribution, then all that's needed is to
include the line in your commit or pull request comment:

```
Signed-off-by: Your Name <your@email.example.org>
```

Git allows you to add this signoff automatically when using the `-s` flag to
`git commit`, which uses the name and email set in your `user.name` and
`user.email` git configs.

If you forgot to sign off your commits before making your pull request and are
on Git 2.17+ you can mass signoff using rebase:

```
git rebase --signoff origin/main
```

## Tips for working on the `matrix-rust-sdk` with specific IDEs

* [RustRover](https://www.jetbrains.com/rust/) will attempt to sync the project
  with all features enabled, causing an error in `matrix-sdk` ("only one of the
  features 'native-tls' or 'rustls-tls' can be enabled"). To work around this,
  open `crates/matrix-sdk/Cargo.toml` in RustRover and uncheck one of the
  `native-tls` or `rustls-tls` feature definitions:

  ![Screenshot of RustRover](.img/rustrover-disable-feature.png)
