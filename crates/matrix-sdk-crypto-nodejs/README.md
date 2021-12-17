# matrix-sdk-crypto-nodejs

## Building

1. Install Rust (latest) and NodeJS (latest LTS preferred).
2. `npm install -g yarn@1` to ensure you have Yarn.
3. `yarn rust:targets` to configure the targets.
4. `yarn build:release` for a release build. `yarn build:debug` for a debug build.

Note that the output will not be capable of a publishable release, but will allow for local
development in the case of a platform-specific binding not being available. Downstream projects
will be affected by this as it might trigger this project's build script during `npm install`.

## Releasing

Releases can only be done from Linux at the moment. Mac OS might work, but is untested.

Windows is only supported through WSL, not from the host.

You will need Docker installed.

1. `yarn publish --new-version 0.2.0` with the appropriate new version set. This will build
   and spin up several Docker containers.
2. Check the release on npm.

## TODO: Release stuff

Windows:
`rustup toolchain install stable-gnu`

use https://www.appveyor.com/ ? 