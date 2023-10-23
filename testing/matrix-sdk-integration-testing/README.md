# Matrix SDK integration test

## Requirements

This requires a synapse backend with a ci patched configuration. You can easily get
it up and running with `docker-compose` via:

```sh
docker-compose -f assets/docker-compose.yml up -d
docker-compose -f assets/docker-compose.yml logs --tail 100 -f
```

**Patches**
You can see the patches we do to configuration (namely activate registration and
resetting rate limits), check out what `assets/ci-start.sh` changes.

## Running

The integration tests can be run with `cargo test` or `cargo nextest run`.

The integration tests expect the environment variables `HOMESERVER_URL` to be the HTTP URL to
access the synapse server and `HOMESERVER_DOMAIN` to be set to the domain configured in
that server. If you are using the provided `docker-compose`, the default will be fine.

## Maintenance

To drop the database of your docker-compose run:

```bash
docker-compose -f assets/docker-compose.yml stop
rm -rf ./assets/data
```
