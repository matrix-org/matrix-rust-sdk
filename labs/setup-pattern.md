# The Setup Pattern

One pattern that bot developers might find useful when using the Matrix Rust SDK is to use a "setup"
pattern, which, simply put, consists of having a bot user bootstrap the bot through a dedicated
`setup` (sub)command, and then run the main executable.

Then, together with that, create an invariant in the main executable: Always expect state.

The latter can be done by checking if the path to a stored and serialized
[`Session`](https://docs.rs/matrix-sdk/latest/matrix_sdk/struct.Session.html) configuration exists
(as created during the `setup` command), and/or by checking for files under the path provided to
[`make_store_config`](https://docs.rs/matrix-sdk/latest/matrix_sdk/store/fn.make_store_config.html).

If the main executable can't find this configuration, abort running.

## Context

The context of this pattern document, and the reason for the crystallization of this idea, is that
currently the "modus operandi" of Instant Messaging bots is quite stateless. Bots might still have
state pertaining to users, databases and whatnot that contain explicit state of business logic, but
it's not expected that the *bot itself* has state.

This expectation becomes especially problematic when combined with E2EE, where the running
application has to not lose sight of it, or else it'll lose access to E2EE (group) chats.

This expectation exists both on the developer and user side of bots, and so to start fixing
it, this pattern is recommended to bot creators, as then the lifetime cycle of the bot
(and its data) is clear to every party involved, and errors are not necessarily "fatal" (or
catastrophic while confusing).

## Consequence

Some of the consequences of the above invariant (setup phase initializes state, run phase expects it initialized) are the following;
- If the bot starts, but cannot find a state directory, it'll abort without implicitly creating one.
- If the setup command is ran while state already exists, it would either abort or offer
  modification (such as a different client URL) to the operator.
- E2EE state cannot easily become wedged or confusingly absent inbetween bot runs, as the run phase
  will not run (and alter E2EE server state, such as device keys) unless it can find the existing
  E2EE state, which should already be initialized.
  - This prevents problems where a bot will try to re-create a device after it lost its state, and
    the new device/login is not verified and thus not trusted in E2EE rooms, which is hard to debug.
  - Instead, the bot will refuse to run at all, preventing it digging itself a deeper ditch, letting
    the operator be notified something is wrong, and either restore the state, or recover it.
- During the setup phase, while the operator is actively interacting with the bot to set it up, an
  E2EE recovery phrase / password / blob could be printed to the terminal for safekeeping, to be used in
  case the original E2EE data ever becomes lost.
  - This would be done during setup instead of a run phase to make: a. make sure the operator sees
    it, and b. if the operator forgets to point the bot's state directories to a persistent file
    system, it has already taken note of the recovery information separately, and so the problem
    with a lost E2EE state can be resolved as long as they still have access to that
    information.

## Extra Notes

This pattern would work well in a docker environment, as a bot developer could recommend the
following;

They can recommend a `docker-compose.yml` file to be used for keeping track of a container
lifecycle, and recommend the `docker-compose run bot setup` command before `docker-compose up -d`,
to give bot operators an easy time setting it up, despite matrix's unconventional stateful nature
(compared to other bot ecosystems).

## What this means for `matrix_sdk`

`matrix_sdk` could help this pattern by building in either a "create" or "open" intent for state,
where it'll error if it can't do either.

Currently I don't see this in `make_store_config`, where it seems to implicitly choose either one,
but if it's made more explicit to *either* open the state database, *or* create it, it would help
guide developers a lot to this pattern, which in turn could guide bot operators to follow this
pattern as well.
