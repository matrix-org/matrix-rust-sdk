This document will try to summarize and describe the thoughts and ideas that go into shaping the
public API for the bot framework SDK.

## Inspirations

#### Discord.py

Discord.py is a python Discord bot/client framework that, together with the expressiveness and
developer "magic" of python, is a pleasant-to-use framework for writing any discord bot.

The main area of inspiration here is `discord.ext.commands`, which provides a framework for handling
commands, but also to handle extensible sub-sections of the program.

##### Commands

Commands are annotated by the `@bot.command()` annotation (`bot` here is a reference to the bot
value, it is sugar for `@commands.command()` + `bot.add_command(func)`), which turns it into a
`Command`, which can then be called on to create `.group()`s of subcommands;

```py
@bot.group()
def say():
    # ...

@say.command()
def hello():
    # ...
```

Above example would already expose the `!say` and `!say hello` commands from the bot, with ease.

##### Cogs

"Cogs" (as in; cogs in a machine/clock) are "submodules" of an app, they're definable subclasses (of
`commands.Cog`) that can be added to the bot instance with `.add_cog()`.

For anyone having worked with an extensible minecraft server (Bukkit, Spigot), Cogs are largely
analogous to plugins therein, with hooks and references to other cogs being possible.

For anyone who hasn't, Cogs are modular pieces of a bot, which can be removed and added at runtime,
and for which other cogs can reference (by name) those cogs, and depend/interact upon them (see
[this example](https://discordpy.readthedocs.io/en/stable/ext/commands/cogs.html#using-cogs))

In discord.py, this is often used to package specific code not dissimilar to crates, and to keep
them logically (largely) separate at runtime.

Cogs can have their own defined listeners and tasks, for which their lifecycle is handled together
with the Cog itself.

Ultimately, Cogs allow for a potential diverse ecosystem of modular pieces of code that can refer to
eachother, and for logically separated codebases, which proves useful when working on large bots.

##### Converters / Parameters

This is somewhat similar to Axum's `FromRequest`, but is also found in dpy;

Discord.py has a concept known as "Converters". It, for `@command()`-annotated functions, allows
grabbing the inputs the user made to its command, and convert those arguments into specific
references.

This is also largely analogous to `click`, which does the same thing with `click.File` and such.

Say;

```py
@command()
async def add(ctx, a: int, b: int):
    print(a + b)
```

This'll do the correct checks to make sure that;
- There are at least two parameters given
- Both parameters can deserialize to an integer

This quickly gets extensible;

```py
@command()
async def joined(ctx, member: discord.Member):
    await ctx.send('{0} joined on {0.joined_at}'.format(member))
```

With this, a user can provide any kind of reference to this server-local member (discord-native user
references, such as `<@1283217987>`, but also; the raw user IDs themselves (`1283217987`), a bare
`username#discrim` in text form, by their bare username, and then by their server nickname)

A number of converters exist, they're listed [here](https://discordpy.readthedocs.io/en/stable/ext/commands/commands.html#discord-converters)

#### Serenity

TBD

#### Axum

##### Extractors

Or; `FromRequest`

These are largely like dpy's Converters, with one major difference; they apply over the whole
input/context, and do not differentiate parameters.

More specifically, axum's `FromRequest` is meant to extract and validate data from requests, making
it easy to quickly set up a path without the boilerplate, see the example below;

```rs
use axum::{
    extract::Json,
    routing::post,
    handler::Handler,
    Router,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct CreateUser {
    email: String,
    password: String,
}

async fn create_user(Json(payload): Json<CreateUser>) {
    // ...
}

let app = Router::new().route("/users", post(create_user));
```

This is illustrating `Json`, which can extract and validate received data from the request body automatically.

A bunch more exist, from anything like Path arguments (grabbing path segments annotated with
`/:arg/`), to Forms.

Axum achieves this pseudo-dependency-injection (and real dependency-injection, through the
`Extension` extractor, which catches values injected through middleware) without macros, because it
simply requires `(T0, T1, T2, T3, ...)` where every `T: FromRequest`.

Possibly, this could be good inspiration for allowing some semblance of DI plus parameter-driven
command-argument parsing, a-la discord.py.

## Ideas

#### Turning d.py's Cogs into a pseudo-actor system

The way by which dpy's Cogs can reference eachother is by grabbing it by name, see the following example;

```py
economy = self.bot.get_cog('Economy')
if economy is not None:
    # do stuff
```

This can get messy if multiple cogs have the same registered name.

(In Bukkit/Spigot, plugins can be queried by their import path, but unlike above approach, JVM has a
stricter control over the global import type/package list, and so collisions are not a concern)

Actor systems have a similar problem; all actors need to be addressable. However, with Rust, actor
systems have largely converged to an idea of `Address<A: Actor>`, where;
- The actor type is known at all times by anyone sending messages, and so message types can be
  checked, and actor interactions.
- Upon drop, a reference counter is decremented, this'd then largely act like an `Arc`

These two quirks allow for a fairly tight implementation of actor systems in rust, abstracting a
large part away, while keeping memory safety.

We could *maybe* use this for cross-cog/module communication, of sorts, where crates wanting to use
another crate's framework-module-integrated services (say, an extensive gamified "bank" system)
would have to import those types and request them by-type explicitly.

*This is experimental, and depends on how we tune and work the module system, if we'd even want that.*

*This is a direct translation from dpy's cog concept, thrown in with bukkit's plugin concept, but
made more rust-friendly and acquinted with concepts from actor systems.*