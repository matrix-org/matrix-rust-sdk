# Redis store for matrix-rust-sdk

## How to use

Here is a minimal example of how to create a matrix-rust-sdk Client that
uses a Redis store to hold its crypto information.

Summary: call `redis_store` on your ClientBuilder instance, passing in a URL for
your Redis server, and a prefix that will be prepended to every key in Redis.

```rust
use matrix_sdk::{config::SyncSettings, Client};

#[tokio::main]
async fn main() {
    let client = Client::builder()
        .homeserver_url("http://localhost:8008")
        .redis_store("redis://127.0.0.1/", None, "my_prefix")
        .await
        .unwrap()
        .build()
        .await
        .unwrap();

    client.login_username("a", "fine I have to").await.unwrap();

    let response = client.sync_once(SyncSettings::default()).await.unwrap();
    println!("{:?}", response);
}
```

## Limitations

Currently, there is no state store, only a crypto store. The crypto store
allows us to retain our identity between executions of the program. Without
a state store, we must re-sync a lot of information from the server every time
we start, which is less efficient.
