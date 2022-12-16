# Jack-in
_your experimental terminal to connect to  matrix via sliding sync_

A simple example client, using sliding sync to [jack in](https://matrix.fandom.com/wiki/Jacking_in) to matrix via the sliding-sync-proxy. 

## Use:

You will need a [running sliding sync proxy](https://github.com/matrix-org/sliding-sync/) for now.

From the roof of this workspace, run jack-in via `cargo run -p jack-in` . Please note that you need to specify the access-token and username, both can be done via environment variables, too. As well as the homeserver (or `http://localhost:8008` will be assumed). See below for how to acquire an access token.

```
Your experimental sliding-sync jack into the matrix

Usage: jack-in [OPTIONS] --user <USER>

Options:
  -p, --password <PASSWORD>
          The password of your account. If not given and no database found, it will prompt you for it [env: JACKIN_PASSWORD=]
      --fresh
          Create a fresh database, drop all existing cache
  -l, --log <LOG>
          RUST_LOG log-levels [env: JACKIN_LOG=] [default: jack_in=info,warn]
  -u, --user <USER>
          The userID to log in with [env: JACKIN_USER=]
      --store-pass <STORE_PASS>
          The password to encrypt the store  with [env: JACKIN_STORE_PASSWORD=]
      --flames <FLAMES>
          Activate tracing and write the flamegraph to the specified file
      --sliding-sync-proxy <PROXY>
          The address of the sliding sync server to connect (probs the proxy) [env: JACKIN_SYNC_PROXY=] [default: http://localhost:8008]
      --full-sync-mode <FULL_SYNC_MODE>
          Activate growing window rather than pagination for full-sync [default: Paging] [possible values: Growing, Paging]
      --limit <LIMIT>
          Limit the growing/paging to this number of maximum items to caonsider "done"
      --batch-size <BATCH_SIZE>
          define the batch_size per request
      --timeline-limit <TIMELINE_LIMIT>
          define the timeline items to load
  -h, --help
          Print help information
```


### Get the access token
1. In [element.io](https://develop.element.org) navigate to `Settings` -> `Help & About`, under _Advanced_ (on the bottom) you can find your Access token
2. Copy it and set as the `JACKIN_SYNC_TOKEN` environment variable or as `--token` cli-parameter on jack-in run
