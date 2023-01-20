# Conventional Commits

This project uses [Conventional
Commits](https://www.conventionalcommits.org/). Read the
[Summary](https://www.conventionalcommits.org/en/v1.0.0/#summary) or
the [Full
Specification](https://www.conventionalcommits.org/en/v1.0.0/#specification)
to learn more.

## Types

Conventional Commits defines _type_ (as in `type(scope):
message`). This section aims at listing the types used inside this
project:

| Type | Definition |
|-|-|
| `feat` | About a new feature. |
| `fix` | About a bug fix. |
| `test` | About a test (suite, case, runner…). |
| `docs` | About a documentation modification. |
| `refactor` | About a refactoring. |
| `ci` | About a Continuous Integration modification. |
| `chore` | About some cleanup, or regular tasks. |

## Scopes

Conventional Commits defines _scope_ (as in `type(scope): message`). This
section aims at listing all the scopes used inside this project:

<table>
  <thead>
    <tr>
      <th>Group</th>
      <th>Scope</th>
      <th>Definition</th>
    </tr>
  </thead>
  <tbody>
    <tr>
      <td rowspan="10">Crates</td>
      <td><code>sdk</code></td>
      <td>About the <code>matrix-sdk</code> crate.</td>
    </tr>
    <tr>
      <td><code>appservice</code></td>
      <td>About the <code>matrix-sdk-appservice</code> crate.</td>
    </tr>
    <tr>
      <td><code>base</code></td>
      <td>About the <code>matrix-sdk-base</code> crate.</td>
    </tr>
    <tr>
      <td><code>common</code></td>
      <td>About the <code>matrix-sdk-common</code> crate.</td>
    </tr>
    <tr>
      <td><code>crypto</code></td>
      <td>About the <code>matrix-sdk-crypto</code> crate.</td>
    </tr>
    <tr>
      <td><code>indexeddb</code></td>
      <td>About the <code>matrix-sdk-indexeddb</code> crate.</td>
    </tr>
    <tr>
      <td><code>qrcode</code></td>
      <td>About the <code>matrix-sdk-qrcode</code> crate.</td>
    </tr>
    <tr>
      <td><code>sled</code></td>
      <td>About the <code>matrix-sdk-sled</code> crate.</td>
    </tr>
    <tr>
      <td><code>store-encryption</code></td>
      <td>About the <code>matrix-sdk-store-encryption</code> crate.</td>
    </tr>
    <tr>
      <td><code>test</code></td>
      <td>About the <code>matrix-sdk-test</code> and <code>matrix-sdk-test-macros</code> crate.</td>
    </tr>
    <tr>
      <td rowspan="4">Bindings</td>
      <td><code>apple</code></td>
      <td>About the <code>matrix-rust-components-swift</code> binding.</td>
    </tr>
    <tr>
      <td><code>crypto-nodejs</code></td>
      <td>About the <code>matrix-sdk-crypto-nodejs</code> binding.</td>
    </tr>
    <tr>
      <td><code>crypto-js</code></td>
      <td>About the <code>matrix-sdk-crypto-js</code> binding.</td>
    </tr>
    <tr>
      <td><code>crypto-ffi</code></td>
      <td>About the <code>matrix-sdk-crypto-ffi</code> binding.</td>
    </tr>
    <tr>
      <td>Labs</td>
      <td><code>sled-state-inspector</code></td>
      <td>About the <code>sled-state-inspector</code> project.</td>
    </tr>
    <tr>
      <td>Continuous Integration</td>
      <td><code>xtask</code></td>
      <td>About the <code>xtask</code> project.</td>
    </tr>
  </tbody>
</table>

## Generating `CHANGELOG.md`

The [`git-cliff`](https://github.com/orhun/git-cliff) project is used
to generate `CHANGELOG.md` automatically. Hence the various
`cliff.toml` files that are present in this project, or the
`package.metadata.git-cliff` sections in various `Cargo.toml` files.

Its companion,
[`git-cliff-action`](https://github.com/orhun/git-cliff-action)
project, is used inside Github Action workflows.
