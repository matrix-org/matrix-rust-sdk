# Matrix SDK integration test

This set of tests requires a Synapse instance, and it runs the tests from this directory against
this real-world server. As a result, these tests depend on the load of the machine/server, and as
such they might be more sensitive to timing issues than other tests in the code base.

## Requirements

This requires a synapse backend with a configuration patched for CI. You can get it up and running
with `docker compose` via:

```sh
docker compose -f assets/docker-compose.yml up -d
docker compose -f assets/docker-compose.yml logs --tail 100 -f
```

Note that this works also with `podman compose`.

**Patches**
You can see the patches we do to configuration (namely activate registration and resetting rate
limits, enable a few experimental features), check out what `assets/ci-start.sh` changes.

## Running

The integration tests can be run with `cargo test` or `cargo nextest run`.

The integration tests expect the environment variables `HOMESERVER_URL` to be the HTTP URL to
access the synapse server and `HOMESERVER_DOMAIN` to be set to the domain configured in
that server. These variables are set to a default value that matches the default `docker-compose.yml`
template; if you haven't touched it, you don't need to manually set those environment variables.

The integration tests can be configured to use a HTTP proxy using the
`MATRIX_SDK_PROXY` environment variable. This plays well with `mitmproxy`
allowing you to inspect the requests tests make to the homeserver.

For example, if you start `mitmproxy` in another terminal, you can see the list
of HTTP requests the test will make by running:

```bash
$ MATRIX_SDK_PROXY=http://localhost:8080 cargo nextest run -p matrix-sdk-integration-testing my_test_name --retries=0
```

## Maintenance

### Delete all instance data

To stop the services and drop the database of your docker-compose cluster, run:

```bash
docker compose -f assets/docker-compose.yml down --volumes --remove-orphans -t 0
```

### Rebuild the synapse image

If the Synapse image has been updated in version, you may need to rebuild the custom Synapse image
using the following:

```bash
docker compose -f assets/docker-compose.yml build --pull synapse
```

Then restart the synapse service:

```bash
docker compose -f assets/docker-compose.yml up -d synapse
```
