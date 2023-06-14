# Upgrades 0.5 ➜ 0.6 

This is a rough migration guide to help you upgrade your code using matrix-sdk 0.5 to the newly released matrix-sdk 0.6 . While it won't cover all edge cases and problems, we are trying to get the most common issues covered. If you experience any other difficulties in upgrade or need support with using the matrix-sdk in general, please approach us in our [matrix-sdk channel on matrix.org][matrix-channel].

## Minimum Supported Rust Version Update: `1.60`

We have updated the minimal rust version you need in order to build `matrix-sdk`, as we require some new dependency resolving features from it:

>  These crates are built with the Rust language version 2021 and require a minimum compiler version of 1.60

## Dependencies

Many dependencies have been upgraded. Most notably, we are using `ruma`  at version `0.7.0` now. It has seen some renamings and restructurings since our last release, so you might find that some Types have new names now.

## Repo Structure Updates

If you are looking at the repository itself, you will find we've rearranged the code quite a bit: we have split out any bindings-specific and testing related crates (and other things) into respective folders, and we've moved all `examples` into its own top-level-folder with each example as their own crate (rendering them easier to find and copy as starting points), all in all slimming down the `crates` folder to the core aspects.


## Architecture Changes / API overall

### Builder Pattern

We are moving to the [builder pattern][] (familiar from e.g. `std::io:process:Command`) as the main configurable path for many aspects of the API, including to construct Matrix-Requests and workflows. This has been and is an on-going effort, and this release sees a lot of APIs transitioning to this pattern, you should already be familiar with from the `matrix_sdk::Client::builder()` in `0.5`. This pattern been extended onto:
 - the [login configuration][login builder] and [login with sso][ssologin builder],
 - [`SledStore` configuratiion][sled-store builder]
 - [`Indexeddb` configuration][indexeddb builder]

Most have fallback (though maybe with deprecation warning) support for an existing code path, but these are likely to be removed in upcoming releases.

### Splitting of concerns: Media

In an effort to declutter the `Client` API dedicated types have been created dealing with specific concerns in one place. In `0.5` we introduced `client.account()`, and `client.encryption()`, we are doing the same with `client.media()` to manage media and attachments in one place with the [`media::Media` type][media typ]  now.

The signatures of media uploads, have also changed slightly: rather than expecting a reader `R: Read + Seek`, it now is a simple `&[u8]`. Which also means no more unnecessary `seek(0)` to reset the cursor, as we are just taking an immutable reference now.

### Event Handling & sync updaes

If you are using the `client.register_event_handler` function to receive updates on incoming sync events, you'll find yourself with a deprecation warning now. That is because we've refactored and redesigned the event handler logic to allowing `removing` of event handlers on the fly, too. For that the new `add_event_handler()` (and `add_room_event_handler`) will hand you an `EventHandlerHandle` (pardon the pun), which you can pass to `remove_event_handler`, or by using the convenient `client.event_handler_drop_guard` to create a `DropGuard` that will remove the handler when the guard is dropped. While the code still works, we recommend you switch to the new one, as we will be removing the `register_event_handler` and `register_event_handler_context` in a coming release.

Secondly, you will find a new [`sync_with_result_callback` sync function][sync with result]. Other than the previous sync functions, this will pass the entire `Result` to your callback, allowing you to handle errors or even raise some yourself to stop the loop. Further more, it will propagate any unhandled errors (it still handles retries as before) to the outer caller, allowing the higher level to decide how to handle that (e.g. in case of a network failure). This result-returning-behavior also punshes through the existing `sync` and `sync_with_callback`-API, allowing you to handle them on a higher level now (rather than the futures just resolving). If you find that warning, just adding a `?` to the `.await` of the call is probably the quickest way to  move forward.

### Refresh Tokens

This release now [supports `refresh_token`s][refresh tokens PR] as part of the [`Session`][session]. It is implemented with a default-flag in serde so deserializing a previously serialized Session (e.g. in a store) will work as before. As part of `refresh_token` support, you can now configure the client via `ClientBuilder.request_refresh_token()` to refresh the access token automagically on certain failures or do it manually by calling `client.refresh_access_token()` yourself. Auto-refresh is _off_ by default.

You can stay informed about updates on the access token by listening to `client.session_tokens_signal()`.

### Further changes

 - [`MessageOptions`][message options] has been updated to Matrix 1.3 by making the `from` parameter optional (and function signatures have been updated, too). You can now request the server sends you messages from the first one you are allowed to have received.
 - `client.user_id()` is not a `future` anymore. Remove any `.await` you had behind it.
 - `verified()`, `blacklisted()` and `deleted()` on `matrix_sdk::encryption::identities::Device` have been renamed with a `is_` prefix.
 - `verified()` on `matrix_sdk::encryption::identities::UserIdentity`, too has been prefixed with `is_` and thus is now called `is_verified()`.
 - The top-level crypto and state-store types of Indexeddb and Sled have been renamed to unique types>
 - `state_store` and `crypto_store` do not need to be boxed anymore when passed to the [`StoreConfig`][store config]
 - Indexeddb's `SerializationError` is now `IndexedDBStoreError`
 - Javascript specific features are now behind the `js` feature-gate
 - The new experimental next generation of sync ("sliding sync"), with a totally revamped api, can be found behind the optional `sliding-sync`-feature-gate


## Quick Troubleshooting

You find yourself focussed with any of these, here are the steps to follow to upgrade your code accordingly:

### warning: use of deprecated associated function `matrix_sdk::Client::register_event_handler`: Use [`Client::add_event_handler`](#method.add_event_handler) instead

As it says on the tin: we have deprecated this function in favor of the newer removable handler approach (see above). You can still continue to use this `fn` for now, but it will be removed in a future release.

### warning: use of deprecated associated function `matrix_sdk::Client::login`: Replaced by [`Client::login_username`](#method.login_username)

We have replaced the login facilities with a `LoginBuilder` and recommend you use that from now on. This isn't an error yet, but the function will be removed in a future release. 

### expected slice `[u8]`, found struct ...

We've updated the `send_attachment` and `Media` signatures to use `&[u8]` rather than `reader: Read + Seek` as it is more convenient and common place for most architectures anyways. If you are using `File::open(path)?` to get that handler, you can just replace that with `std::fs::read(path)?`

### no method named `verified` found for struct `matrix_sdk::encryption::identities::Device` in the current scope

Boolean flags like `verified`, `deleted`, `blacklisted`, etc have been renamed with a `is_` prefix. So, just follow the cargo suggestion:
```
   |
69 |             device.verified()
   |                    ^^^^^^^^ help: there is an associated function with a similar name: `is_verified`
 ```

 ### unresolved import `matrix_sdk::ruma::events::AnySyncRoomEvent`

 Ruma has been updated to `0.7.0`, you will find some ruma Events names have changed, most notably, the `AnySyncRoomEvent` is now named `AnySyncTimelineEvent` (and not `AnySyncStateEvent`, which cargo wrongly suggests). Just rename the import and usage of it.

### `std::option::Option<&matrix_sdk::ruma::UserId>` is not a future

You are seeing something along the lines of:
```
19 |     if room_member.state_key != client.user_id().await.unwrap() {
   |                                                 ^^^^^^ `std::option::Option<&matrix_sdk::ruma::UserId>` is not a future
   |
   = help: the trait `Future` is not implemented for `std::option::Option<&matrix_sdk::ruma::UserId>`
   = note: std::option::Option<&matrix_sdk::ruma::UserId> must be a future or must implement `IntoFuture` to be awaited
   = note: required because of the requirements on the impl of `IntoFuture` for `std::option::Option<&matrix_sdk::ruma::UserId>`
help: remove the `.await`
   |
19 -     if room_member.state_key != client.user_id().await.unwrap() {
19 +     if room_member.state_key != client.user_id().unwrap() {
```

You are using `client.user_id().await` but `user_id()` is no longer `async`. Just follow the cargo suggestion and remove the `.await`, it is not necessary any longer.


 [matrix-channel]: https://matrix.to/#/#matrix-rust-sdk:matrix.org
 [builder pattern]: https://doc.rust-lang.org/1.0.0/style/ownership/builders.html
 [login builder]: https://docs.rs/matrix-sdk/latest/matrix_sdk/struct.LoginBuilder.html
 [ssologin builder]: https://docs.rs/matrix-sdk/latest/matrix_sdk/struct.SsoLoginBuilder.html
 [sled-store builder]: https://docs.rs/matrix-sdk-sled/latest/matrix_sdk_sled/struct.SledStateStoreBuilder.html
 [indexeddb builder]: https://docs.rs/matrix-sdk-indexeddb/latest/matrix_sdk_indexeddb/struct.IndexeddbStateStoreBuilder.html
 [media type]: https://docs.rs/matrix-sdk/latest/matrix_sdk//media/struct.Media.html
 [sync with result]: https://docs.rs/matrix-sdk/latest/matrix_sdk/struct.Client.html#method.sync_with_result_callback
 [session]: https://docs.rs/matrix-sdk/latest/matrix_sdk/struct.Session.html
 [refresh tokens PR]: https://github.com/matrix-org/matrix-rust-sdk/pull/892
 [store config]:  https://docs.rs/matrix-sdk-base/latest/matrix_sdk_base/store/struct.StoreConfig.html
 [message options]: https://docs.rs/matrix-sdk/latest/matrix_sdk/room/struct.MessagesOptions.html
