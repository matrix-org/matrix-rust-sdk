# `matrix-sdk-crypto-nodejs`

Welcome to the [Node.js] binding for the Rust [`matrix-sdk-crypto`]
library! This binding is part of the [`matrix-rust-sdk`] project,
which is a library implementation of a [Matrix] client-server.

`matrix-sdk-crypto-nodejs` is a no-network-IO implementation of a
state machine, named `OlmMachine`, that handles E2EE ([End-to-End
Encryption](https://en.wikipedia.org/wiki/End-to-end_encryption)) for
[Matrix] clients.

## Usage

Just add the latest release to your `package.json`:

```sh
$ npm install --save @matrix-org/matrix-sdk-crypto-nodejs
```

When installing, NPM will download the corresponding prebuilt Rust library for your current host system. The following are supported:

<table>
  <thead>
    <tr>
      <th>Platform</th>
      <th>Architecture</th>
      <th>Triple</th>
      <th>Prebuilt available</th>
    </tr>
  </thead>
  <tbody>
    <tr>
      <td rowspan="5">Linux</td>
      <td rowspan="2"><code>aarch</code></td>
      <td><code>aarch64-unknown-linux-gnu</code></td>
      <td>✅</td>
    </tr>
    <tr>
      <td><code>arm-unknown-linux-gnueabihf</code></td>
      <td>✅</td>
    </tr>
    <tr>
      <td rowspan="3"><code>amd</code></td>
      <td><code>x86_64-unknown-linux-gnu</code></td>
      <td>✅</td>
    </tr>
    <tr>
      <td><code>x86_64-unknown-linux-musl</code></td>
      <td>✅</td>
    </tr>
    <tr>
      <td><code>i686-unknown-linux-gnu</code></td>
      <td>✅</td>
    </tr>
    <tr>
      <td rowspan="2">macOS</td>
      <td><code>aarch</code></td>
      <td><code>arch64-apple-darwin</code></td>
      <td>✅</td>
    </tr>
    <tr>
      <td><code>amd</code></td>
      <td><code>x86_64-apple-darwin</code></td>
      <td>✅</td>
    </tr>
    <tr>
      <td rowspan="3">Windows</td>
      <td><code>aarch</code></td>
      <td><code>aarch64-pc-windows-msvc</code></td>
      <td>✅</td>
    </tr>
    <tr>
      <td rowspan="2"><code>amd</code></td>
      <td><code>x86_64-pc-windows-msvc</code></td>
      <td>✅</td>
    </tr>
    <tr>
      <td><code>i686-pc-windows-msvc</code></td>
      <td>✅</td>
    </tr>
  </tbody>
</table>

## Development

This Node.js binding is written in [Rust]. To build this binding, you
need to install the Rust compiler, see [the Install Rust
Page](https://www.rust-lang.org/tools/install). Then, the workflow is
pretty classical by using [npm], see [the Downloading and installing
Node.js and npm
Page](https://docs.npmjs.com/downloading-and-installing-node-js-and-npm).

The binding is compatible with, and tested against, the Node.js
versions that are in “current”, “active” or “maintenance” states,
according to [the Node.js Releases
Page](https://nodejs.org/en/about/releases/), _and_ which are
compatible with [NAPI v6 (Node.js
API)](https://nodejs.org/api/n-api.html#node-api-version-matrix). It
means that this binding will work with the following versions: 16.0.0,
18.0.0, 19.0.0 and 20.0.0.

Once the Rust compiler, Node.js and npm are installed, you can run the
following commands:

```sh
$ npm install --ignore-scripts
$ npm run build
$ npm run test
```

An `index.js`, `index.d.ts` and a `*.node` files should be
generated. At the same level of those files, you can edit a file and
try this:

```javascript
const { OlmMachine } = require("./index.js");

// Let's see what we can do.
```

The `OlmMachine` state machine works in a push/pull manner:

-   You push state changes and events retrieved from a Matrix homeserver
    `/sync` response, into the state machine,
-   You pull requests that you will need to send back to the homeserver
    out of the state machine.

```javascript
const { OlmMachine, UserId, DeviceId, RoomId, DeviceLists } = require("./index.js");

async function main() {
    // Define a user ID.
    const alice = new UserId("@alice:example.org");

    // Define a device ID.
    const device = new DeviceId("DEVICEID");

    // Let's create the `OlmMachine` state machine.
    const machine = await OlmMachine.initialize(alice, device);

    // Let's pretend we have received changes and events from a
    // `/sync` endpoint of a Matrix homeserver, …
    const toDeviceEvents = "[]"; // JSON-encoded list of events
    const changedDevices = new DeviceLists();
    const oneTimeKeyCounts = {};
    const unusedFallbackKeys = [];

    // … and push them into the state machine.
    const decryptedToDevice = await machine.receiveSyncChanges(
        toDeviceEvents,
        changedDevices,
        oneTimeKeyCounts,
        unusedFallbackKeys,
    );

    // Now, let's pull requests that we need to send out to the Matrix
    // homeserver.
    const outgoingRequests = await machine.outgoingRequests();

    // To complete the workflow, send the requests here out and call
    // `machine.markRequestAsSent`.
}

main();
```

### With tracing (experimental)

If you want to enable [tracing](https://tracing.rs), i.e. to get the
logs, you should re-compile the extension with the `tracing` feature
turned on:

```sh
$ npm run build -- --features tracing
```

Now, you can use the `MATRIX_LOG` environment variable to tweak the log filtering, such as:

```sh
$ MATRIX_LOG=debug npm run test
```

See
[`tracing-subscriber`](https://tracing.rs/tracing_subscriber/index.html)
to learn more about the `RUST_LOG`/`MATRIX_LOG` environment variable.

#### Using tracing in a development environment

To use tracing in client applications that import these bindings, here's how to do it in
a local development environment:

- In this directory, run `npm link` to make your local build of the bindings available to
other Node projects on your system
- In your client app's source directory, run `npm link @matrix-org/matrix-sdk-crypto-nodejs`
to make it use your trace-enabled local build of the bindings
- In your client app's source code, add a call to `initTracing` near startup time
- Run your app with the `MATRIX_LOG` environment variable set to the desired log level

Either `npm link` command may be substituted with `yarn link`.

## Documentation

[The documentation can be found
online](https://matrix-org.github.io/matrix-rust-sdk/bindings/matrix-sdk-crypto-nodejs/).

To generate the documentation locally, please run the following
command:

```sh
$ npm run doc
```

The documentation is generated in the `./docs` directory.

[Node.js]: https://nodejs.org/
[`matrix-sdk-crypto`]: https://github.com/matrix-org/matrix-rust-sdk/tree/main/crates/matrix-sdk-crypto
[`matrix-rust-sdk`]: https://github.com/matrix-org/matrix-rust-sdk
[Matrix]: https://matrix.org/
[Rust]: https://www.rust-lang.org/
[npm]: https://www.npmjs.com/
