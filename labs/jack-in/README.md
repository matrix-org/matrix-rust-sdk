# Jack-in
_your experimental terminal to connect to  matrix via sliding sync_

A simple example client, using sliding sync to [jack in](https://matrix.fandom.com/wiki/Jacking_in) to matrix via the sliding-sync-proxy. 

## Use:

You will need a [locally running sliding sync proxy]() for now.

### Get the access token

1. Login with a fresh session on [element.io](https://develop.element.org)
2. verify the session against your existing session via key-cross-signing
3. navigate to `Settings` -> `Help & About`, under _Advanced_ (on the bottom) you can find your Access token
4. copy it and run jack-in via (where `$TOKEN` is your token):
```
cargo run -p jack-in -- --token $TOKEN
```