# SQL StateStore for matrix-sdk

![Build Status](https://img.shields.io/github/workflow/status/DarkKirb/matrix-sdk-statestore-sql/Build%20checks)
[![Code Coverage](https://img.shields.io/coveralls/github/DarkKirb/matrix-sdk-statestore-sql)](https://coveralls.io/github/DarkKirb/matrix-sdk-statestore-sql)
[![License](https://img.shields.io/badge/License-Apache%202.0-yellowgreen.svg)](https://opensource.org/licenses/Apache-2.0)


This crate allows you to use your postgres/mysql/sqlite/mssql database as a state and crypto store for matrix-sdk.

## Features

- `rustls`: Enables the rustls TLS backend in sqlx and matrix-sdk
- `native-tls`: Enables the native-tls TLS backend in sqlx and matrix-sdk (enabled by default)
- `postgres`: Enables support for postgres databases (enabled by default)
- `mysql`: Enables support for mysql databases
- `sqlite`: Enables support for sqlite databases
- `encryption` Enables the CryptoStore

Exactly one of `rustls` and `native-tls` need to be enabled. At least one of `postgres`, `mysql`, `sqlite`, or `mssql` must be enabled.
## Minimum Supported Rust Version
The MSRV is currently 1.54.0.

Increasing the MSRV is a breaking change. It is expected that the MSRP will rise to 1.60.0 in the next release.
## Authors

- [Charlotte](https://github.com/DarkKirb)

## License

This project is licensed under the Apache-2.0 License - see the [LICENSE.md](LICENSE.md) file for details

## Acknowledgments

- [awesome-readme](https://github.com/matiassingers/awesome-readme)
- [Matrix Rust SDK](https://github.com/matrix-org/matrix-rust-sdk)

## Contributors

<!-- readme: contributors -start -->
<table>
<tr>
    <td align="center">
        <a href="https://github.com/DarkKirb">
            <img src="https://avatars.githubusercontent.com/u/23011243?v=4" width="100;" alt="DarkKirb"/>
            <br />
            <sub><b>Charlotte</b></sub>
        </a>
    </td></tr>
</table>
<!-- readme: contributors -end -->
