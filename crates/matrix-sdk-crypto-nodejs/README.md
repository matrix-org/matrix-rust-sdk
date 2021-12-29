# matrix-sdk-crypto-nodejs

## Building

1. Install Rust (latest) and NodeJS (latest LTS preferred).
2. `npm install -g yarn@1` to ensure you have Yarn.
3. `yarn install` to configure dependencies.
4. `yarn rust:targets` to configure the targets.
5. `yarn build:release` for a release build. `yarn build:debug` for a debug build.
6. `yarn build:ts` to build the TypeScript part.

Note that the output will not be capable of a publishable release, but will allow for local
development in the case of a platform-specific binding not being available. Downstream projects
will be affected by this as it might trigger this project's build script during `npm install`.

## Releasing

Note that the release process currently only works on Linux. Mac OS might work, but Windows definitely
doesn't. WSL works fine though, just not on the host.

You will need Docker installed.

1. Commit *and push* all relevant changes to the repo. The push is important as the build process
   relies upon commit hashes.
2. Update the relevant commit hashes in the `Cargo.toml` file, and ensure they are being used. Push
   these changes.
3. Update the `package.json` version. `npm version` may be of use.
4. Run `npm publish`. This will build and set up various Docker containers.
   * **Do not use yarn to publish, as it might publish the wrong thing.**

## TODO: Release stuff

Windows:
`rustup toolchain install stable-gnu`

use https://www.appveyor.com/ ? 
