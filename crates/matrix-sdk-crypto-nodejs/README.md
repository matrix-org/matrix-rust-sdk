# `matrix-sdk-crypto-nodejs`

Welcome to the [Node.js] binding for the Rust [`matrix-sdk-crypto`]
library! This binding is part of the [`matrix-rust-sdk`] project,
which is a library implementation of a [Matrix] client-server.

`matrix-sdk-crypto-nodejs` is a no-network-IO implementation of a
state machine, named `OlmMachine`, that handles E2EE ([End-to-End
Encryption](https://en.wikipedia.org/wiki/End-to-end_encryption)) for
[Matrix] clients.

## Usage

This Node.js binding is written in [Rust]. To build this binding, you
need to install the Rust compiler, see [the Install Rust
Page](https://www.rust-lang.org/tools/install). Then, the workflow is
pretty classical by using [npm], see [the Downloading and installing
Node.js and npm
Page](https://docs.npmjs.com/downloading-and-installing-node-js-and-npm).

The binding is using NAPI 6 (Node API version 6), which means that is
compatible with Node.js versions 10.20.0, 12.17.0, 14.0.0, 15.0.0 and
16.0.0 (see [the full Node API version
matrix](https://nodejs.org/api/n-api.html#node-api-version-matrix)).

Once the Rust compiler, Node.js and npm are installed, you can run the
following commands:

```sh
$ npm install
$ npm run build
$ npm run test
```

An `index.js`, `index.d.ts` and a `*.node` files should be
generated. At the same level of those files, you can edit a file and
try this:

```javascript
const { OlmMachine } = require('./index.js');

// Let's see what we can do.
```

The `OlmMachine` state machine works in a push/pull manner:

* You push state changes and events retrieved from a Matrix homeserver
  `/sync` response, into the state machine,
  
* You pull requests that you will need to send back to the homeserver
  out of the state machine.
  
```javascript
const { OlmMachine, UserId, DeviceId, RoomId, DeviceLists } = require('./index.js');

async function main() {
    // Define a user ID.
    const alice = new UserId('@alice:example.org');

    // Define a device ID.
    const device = new DeviceId('DEVICEID');

    // Let's create the `OlmMachine` state machine.
    const machine = await OlmMachine.initialize(alice, device);

    // Let's pretend we have received changes and events from a
    // `/sync` endpoint of a Matrix homeserver, …
    const toDeviceEvents = "{}"; // JSON-encoded
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

Now, you can use the `RUST_LOG` environment variable to tweak the log filtering, such as:

```sh
$ RUST_LOG=debug npm run test
```

See
[`tracing-subscriber`](https://tracing.rs/tracing_subscriber/index.html)
to learn more about the `RUST_LOG` environment variable.

## Documentation

TBD.

[Node.js]: https://nodejs.org/
[`matrix-sdk-crypto`]: https://github.com/matrix-org/matrix-rust-sdk/tree/main/crates/matrix-sdk-crypto
[`matrix-rust-sdk`]: https://github.com/matrix-org/matrix-rust-sdk
[Matrix]: https://matrix.org/
[Rust]: https://www.rust-lang.org/
[npm]: https://www.npmjs.com/
