# Jack-in
_your experimental terminal to connect to  matrix via sliding sync_

A simple example client, using sliding sync to [jack in](https://matrix.fandom.com/wiki/Jacking_in) to matrix via the sliding-sync-proxy. 

## Use:

You will need a [running sliding sync proxy](https://github.com/matrix-org/sliding-sync/) for now.

From the roof of this workspace, run jack-in via `cargo run -p jack-in` . Please note that you need to specify the access-token and username, both can be done via environment variables, too. As well as the homeserver (or `http://localhost:8008` will be assumed). See below for how to acquire an access token.


### Get the access token
1. In [element.io](https://develop.element.org) navigate to `Settings` -> `Help & About`, under _Advanced_ (on the bottom) you can find your Access token
2. Copy it and set as the `JACKIN_SYNC_PROXY` environment variable or as `--sliding-sync-proxy` cli-parameter on jack-in run
