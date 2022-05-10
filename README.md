# SQL StateStore for matrix-sdk

![Build Status](https://img.shields.io/github/workflow/status/DarkKirb/matrix-sdk-statestore-sql/Build%20checks)
[![Code Coverage](https://img.shields.io/coveralls/github/DarkKirb/matrix-sdk-statestore-sql)](https://coveralls.io/github/DarkKirb/matrix-sdk-statestore-sql)
[![License](https://img.shields.io/badge/License-Apache%202.0-yellowgreen.svg)](https://opensource.org/licenses/Apache-2.0)
[![Docs - Main](https://img.shields.io/badge/docs-main-blue.svg?style=flat-square)](https://darkkirb.github.io/matrix-sdk-statestore-sql/rust/matrix_sdk_statestore_sql/)

This crate allows you to use your postgres/mysql/sqlite database as a state and crypto store for matrix-sdk.

## Crate Features

- `rustls`: Enables the rustls TLS backend in sqlx and matrix-sdk
- `native-tls`: Enables the native-tls TLS backend in sqlx and matrix-sdk (enabled by default)
- `postgres`: Enables support for postgres databases (enabled by default)
- `mysql`: Enables support for mysql databases
- `sqlite`: Enables support for sqlite databases
- `encryption` Enables the CryptoStore

Exactly one of `rustls` and `native-tls` need to be enabled. At least one of `postgres`, `mysql`, or `sqlite` must be enabled.

### Mysql backend

The mysql backend is very different from the postgres and sqlite backends.
While it is actively tested to work, it may not behave the same way as other backends.
This is because mysql does not support some standard SQL database features that even sqlite supports.

#### Media storage

The Media storage caches matrix media so that it doesn’t have to be refetched from the internet.

On postgres and sqlite, the media storage table is used as an LRU cache with a fixed size. Mysql however does not support `LIMIT` in subqueries and therefore it will delete items that have not been accessed in the past 5 minutes.

This means that for large installations it may not be advisable to use mysql, as a large amount of media events (for example due to joining a large room) will massively increase the database size, which will not happen with postgres and sqlite.

## Minimum Supported Rust Version
The MSRV is currently 1.60.0.

Increasing the MSRV is a breaking change.
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
