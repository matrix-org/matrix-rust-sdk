# Upgrades 0.5 -> 0.6 

This is a rough migration guide to help you upgrade your code using matrix-sdk 0.5 to the newly released matrix-sdk 0.6 . While it won't cover all edge cases and problems, we are trying to get the most common issues covered. If you experience any other difficulties in upgrade or need support with using the matrix-sdk in general, please approach us in our [matrix-sdk channel on matrix.org][matrix-channel].

## Minimum Supported Rust Version Update: `1.60`

We have updated the minimal rust version you need in order to build `matrix-sdk`, as we require some new depencency resolving features from it: 

>  These crates are built with the Rust language version 2021 and require a minimum compiler version of 1.60


## Repo Structure Updates

If you are looking at the repository itself, you will find we've rearranged the code quite a bit: we have split out any bindings-specific and testing related crates (and other things) into respective folders, and we've moved all `examples` into its own top-level-folder with each example as their own crate (rendering them easier to find and copy as starting points), all in all slimming down the `crates` folder to the core aspects.


## Architecture Changes / API overall

### Builder Pattern

We are moving to the [builder pattern][] (familiar from e.g. `std::io:process:Command`) as the main configurable path for many aspects of the API, including to construct Matrix-Requests and workflows. This has been and is an on-going effort, and this release sees a lot of APIs transitioning to this pattern, you should already be familiar with from the `matrix_sdk::Client::builder()` in `0.5`. This pattern been extended onto:
 - the [login configuration][login builder] and [login with sso][ssologin builder],
 - [`SledStore` configuratiion][sled-store builder]
 - [`Indexeddb` configuration][indexeddb builder]

Most have fallback (though maybe with deprecation warning) support for an existing code path, but these are likley to be removed in upcoming releases.

### Splitting of concerns: Media

In an effort to declutter the `Client` API dedicated types have been created dealing with specfic concerns in one place. In `0.5` we introduced `client.account()`, and `client.encryption()`, we are doing the same with `client.media()` to manage media and attachments in one place with the [`media::Media` type][media typ]  now.

### Event Handling & sync updaes

If you are using the `client.register_event_handler` function to receive updates on uncoming sync events, you'll find yourself with a deprecation warning now. That is because we've refactored and redsigned the event handler logic to allowing `removing` of event handlers on the fly, too. For that the new `add_event_handler()` (and `add_room_event_handler`) will hand you an `EventHandlerHandle` (pardon the pun), which you can pass to `remove_event_handler`. While the code still works, we recommend you switch to the new one, as we will be removing the `register_event_handler` and `register_event_handler_context` in a coming release.

Secondly, you will find a new [`sync_with_result_callback` sync function][sync with result]. Other than the previous sync functions, this will pass the entire `Result` to your callback, allowing you to handle errors or even raise some yourself to stop the loop. Further more, it will propagate any unhandled errors (it still handles retries as before) to the outer caller, allowing the higher level to decide how to handle that (e.g. in case of a network failure).

### Refresh Tokens

This release now [supports `refresh_token`s][refresh tokens PR] as part of the [`Session`][session]. It is implemented with a default-flag in serde so deserializing a previously serialized Session (e.g. in a store) will work as before. As part of `refresh_token` support, you can now configure the client via `ClientBuilder.request_refresh_token()` to refresh the access token automagicaly on certain failures or do it manually by calling `client.refresh_access_token()` yourself. Auto-refresh is _off_ by default.

You can stay informed about updates on the access token by listening to `client.session_tokens_signal()`.


## Quick Troubleshooting

You find yourself focussed with any of these, here are the steps to follow to upgrade your code accordingly:



 
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
