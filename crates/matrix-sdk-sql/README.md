# SQL StateStore for matrix-sdk

![Build Status](https://img.shields.io/github/workflow/status/DarkKirb/matrix-sdk-statestore-sql/Build%20checks)
[![Code Coverage](https://img.shields.io/coveralls/github/DarkKirb/matrix-sdk-statestore-sql)](https://coveralls.io/github/DarkKirb/matrix-sdk-statestore-sql)
[![License](https://img.shields.io/crates/l/matrix-sdk-sql)](https://opensource.org/licenses/Apache-2.0)
[![Docs - Main](https://img.shields.io/badge/docs-main-blue.svg)](https://darkkirb.github.io/matrix-sdk-statestore-sql/rust/matrix_sdk_sql/)
[![Version](https://img.shields.io/crates/v/matrix-sdk-sql)](https://crates.io/crates/matrix-sdk-sql)

This crate allows you to use your postgres/sqlite database as a state and crypto store for matrix-sdk.

## Crate Features

- `rustls`: Enables the rustls TLS backend in sqlx and matrix-sdk
- `native-tls`: Enables the native-tls TLS backend in sqlx and matrix-sdk (enabled by default)
- `postgres`: Enables support for postgres databases (enabled by default)
- `sqlite`: Enables support for sqlite databases
- `e2e-encryption` Enables the CryptoStore

Exactly one of `rustls` and `native-tls` need to be enabled. At least one of `postgres` or `sqlite` must be enabled.

## Usage

This crate integrates with your existing [SQLx](https://github.com/launchbadge/sqlx) database pool.

```rust

let sql_pool: Arc<sqlx::Pool<DB>> = /* ... */;
// Create the  store config
let store_config = matrix_sdk_sql::store_config(sql_pool, Some(std::env::var("MYAPP_SECRET_KEY")?)).await?;

```

After that you can pass it into your client builder as follows:

```rust
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

If you are using the `store_config` function, the store will be automatically unlocked for you.

## Authors

- [Charlotte](https://github.com/DarkKirb)

## License

This project is licensed under the Apache-2.0 License - see the [LICENSE.md](LICENSE.md) file for details
