# SQL StateStore for matrix-sdk

![Build Status](https://img.shields.io/github/workflow/status/DarkKirb/matrix-sdk-statestore-sql/Build%20checks)
[![Code Coverage](https://img.shields.io/coveralls/github/DarkKirb/matrix-sdk-statestore-sql)](https://coveralls.io/github/DarkKirb/matrix-sdk-statestore-sql)
[![License](https://img.shields.io/crates/l/matrix-sdk-sql)](https://opensource.org/licenses/Apache-2.0)
[![Docs - Main](https://img.shields.io/badge/docs-main-blue.svg)](https://darkkirb.github.io/matrix-sdk-statestore-sql/rust/matrix_sdk_statestore_sql/)
[![Version](https://img.shields.io/crates/v/matrix-sdk-sql)](https://crates.io/crates/matrix-sdk-sql)

This crate allows you to use your postgres/sqlite database as a state and crypto store for matrix-sdk.

## Crate Features

- `rustls`: Enables the rustls TLS backend in sqlx and matrix-sdk
- `native-tls`: Enables the native-tls TLS backend in sqlx and matrix-sdk (enabled by default)
- `postgres`: Enables support for postgres databases (enabled by default)
- `sqlite`: Enables support for sqlite databases
- `e2e-encryption` Enables the CryptoStore

Exactly one of `rustls` and `native-tls` need to be enabled. At least one of `postgres` or `sqlite` must be enabled.

## Minimum Supported Rust Version
The MSRV is currently 1.60.0.

Increasing the MSRV is a breaking change.

## Usage

This crate integrates with your existing [SQLx](https://github.com/launchbadge/sqlx) database pool.

```rust

let sql_pool: Arc<sqlx::Pool<DB>> = /* ... */;
// Create the state store, applying migrations if necessary
let state_store = StateStore::new(&sql_pool).await?;

```

After that you can pass it into your client builder as follows:

```rust
let store_config = StoreConfig::new().state_store(Box::new(state_store));

let client_builder = Client::builder()
                    /* ... */
                     .store_config(store_config)
```

### CryptoStore

Enabling the `e2e-encryption` feature enables cryptostore functionality. To protect encryption session information, the contents of the tables are encrypted in the same manner as in `matrix-sdk-sled`.

Before you can use cryptostore functionality, you need to unlock the cryptostore:

```rust
let mut state_store = /* as above */;

state_store.unlock_with_passphrase(std::env::var("MYAPP_SECRET_KEY")?).await?;
```

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
