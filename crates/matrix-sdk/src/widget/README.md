# Widget Driver

This module implements a widget driver. The widget driver allows a client to give widgets (webviews) access to Matrix via
a specified `postMessage` API.
Details about the features implemented by this driver can be found in the following MSC's:

- [MSC3803: Matrix Widget API v2](https://github.com/matrix-org/matrix-spec-proposals/issues/3803)
- [MSC2762: Allowing widgets to send/receive events](https://github.com/matrix-org/matrix-spec-proposals/blob/travis/msc/widgets-send-receive-events/proposals/2762-widget-event-receiving.md)
- [MSC4157: Delayed Events (widget api)](https://github.com/matrix-org/matrix-spec-proposals/pull/4157)
- [MSC3819: Allowing widgets to send/receive to-device messages](https://github.com/matrix-org/matrix-spec-proposals/pull/3819)

It supports sending and reading events and provides some rudimentary client navigation features.
There are some additional actions:

- Get an OpenID token (not OAuth) to identify a user.
- Ask for supported API versions.
- Inform the client that the widget has loaded its content and is ready.

## The Widget Api

_from [MSC3803: Matrix Widget API v2](https://github.com/matrix-org/matrix-spec-proposals/issues/3803)_

The widget postMessage API is split into two halves:

- Matrix client (e.g. Element) postMessage API (api: “fromWidget”) -- This API listens within
  a Matrix client for inbound messages from widgets.
- Widget client postMessage API (api: “toWidget”) -- This API listens within
  a widget for inbound messages coming from the Matrix client, or the container embedding the widget.

The API follows a request/response pattern. So each request can be sent in both direction.
This makes for four valid `postMessage` structures:

- Widget -> Client: `api: FromWidget`, ~~`response:`~~
- Client -> Widget: `api: FromWidget`, `response: {...}`
- Client -> Widget: `api: ToWidget`, ~~`response:`~~
- Widget -> Client: `api: ToWidget`, `response: {...}`

Both APIs conform to the same basic structure:

An example request / response:

```javascript
{
    api: $API_NAME,
    requestId: $REQUEST_ID,
    action: $ACTION,
    widgetId: $WIDGET_ID,
    data: {}
}
```

The complete request object is returned to the caller with an additional `response` key like so:

```json
{
    api: $API_NAME,
    requestId: $REQUEST_ID,
    action: $ACTION,
    widgetId: $WIDGET_ID,
    data: {},
    response: { ... }
}
```

The `api` field is required on all requests and must be either `"fromWidget"` or `"toWidget"` depending upon the direction
of messaging.

The "requestId" field should be unique and included in all requests. This field has been omitted from all of the
following examples, for the sake of brevity.

The "action" determines the format of the request and response. All actions can return an error response.

The "widgetId" field denotes the widget that the request originates from and is required.

Additional data can be sent as arbitrary fields. However, typically the "data" object should be used.

A success response is an object with zero or more keys where none of keys are “error”.

An error response is a "response" object which consists of a sole "error" key to indicate an error.

They look like:

```json
{
    ...
    error: {
        message: "Unable to invite user into room.",
        <Original Error Object>
    }
}
```

The "message" key should be a human-friendly string.

## `WidgetDriver` internals

Internally the driver consists of two main message types:

- [`machine::IncomingMessage`] collects all possible "input" information which can come from the [`MatrixDriver`]
  or the [`WidgetDriverHandle`] channel, that passes over the actions from the widget.
  - `WidgetMessage`: An incoming raw message from the widget.
  - `MatrixDriverResponse`: A response to a request from the `WidgetMachine` to the `MatrixDriver`.
    For example if the `WidgetMachine` requests the `MatrixDriver` to get approved capabilities,
    the response would contain what capabilities actually were approved.
  - `MatrixEventReceived`: The `MatrixDriver` notified the `WidgetMachine` of a new Matrix event.
- [`machine::Action`] describes an operation that the `WidgetDriver` wants to perform:
  - `SendToWidget`: Send a raw message to the widget.
  - `MatrixDriverRequest`: A command sent from the client widget API state machine to the
    client (driver). Once the command is executed,
    the client will typically generate an `MatrixDriverResponse` with the result.
  - `Subscribe`: Subscribe to the events that the widget capabilities allow,
    in the _current_ room, i.e. a room which this widget is instantiated with.
    The client is aware of the room.
  - `Unsubscribe`: Unsubscribe from the events that the widget capabilities allow,
    in the _current_ room. Symmetrical to `Subscribe`.

```ascii
                                       Public API ──╮                                                      
                                                    ▼──────────────────────────────────────────────────┐   
                                                    │ WidgetDriver                                     │   
┌────────────────┐  ToWidget response: -            │            ┌──────────────────────────────────┐  │   
│                │  FromWidget response: {...}      │            │                                  │  │   
│                ◄───────────────────╮              │            │                                  │  │   
│                │                   │              │            │           WidgetMachine          │  │   
│                │                 ┌─┴──────────────┴─────┐      │                                  │  │   
│                │    PostMessage  │                      │────╮ │                                  │  │   
│     Widget     │    Widget<->App │  WidgetDriverHandle  │    │ └──────┬─────────────────▲─────────┘  │   
│                │                 │                      ├──╮ │        │                 │            │   
│                │                 └─▲──────────────┬─────┘  │ │        │ Action          │ IncomingMessage
│                ├───────────────────╯              │        │ │        ▼                 │            │   
│                │  FromWidget response: -          │        │ │     │ │ │ │             ▲ ▲           │   
│                │  ToWidget response: {...}        │        │ │     │ │ │ │             │ │           │   
└────────────────┘                                  │        │ ╰─────╯ │ │ │             │ │           │   
                                   ┌────────────────┴─────┐  ╰─────────┼─┼─┼─────────────╯ │           │   
                                   │ CapabilitiesProvider │            │ │ │               ╰──────╮    │   
                                   └────────────────┬─────┘            │ │ │                      │    │   
                                                    │                  │ │ │                      │    │   
                                   ┌────────────────┴─────┐            │ │ │                      │    │   
                                   │       Settings       │            │ │ │                      │    │   
                                   └────────────────┬─────┘         ┌──▼─▼─▼──────────────────────┴───┐│   
                                   ┌────────────────┴─────┐         │                                 ││   
                                   │                      │         │                                 ││   
                                   │                      │   ╭─────┤          MatrixDriver           ││   
┌─────────────────────────────┐    │         Room         │   │     │                                 ││   
│                             │    │                      ◄───╯     │                                 ││   
│                             │    │                      │         └─────────────────────────────────┘│   
│                             │    └──────────┬─────┬─────┘                                            │   
│                             │               │     └──────────────────────────────────────────────────┘   
│       Rust SDK client       ◄───────────────╯                                                            
│                             │                                                                            
│                             │                                                                            
│                             │                                                                            
│                             │                                                                            
└─────────────────────────────┘                                                                            
```

The `WidgetDriver` itself is responsible for:

- processing the `Action`s (capability checks) and feeding new `IncomingMessage` to the `WidgetMachine`
- doing the serialization,
- spawning the different loops (`MatrixDriver`, `WidgetMachine`),
- Exposing the channels `WidgetDriverHandle` for the client to hook up the widget,

The `WidgetMachine` is a no IO state machine responsible for the stateful logic.

- Initialization procedure
  - capability negotiation
  - wait for the widget to be ready
  - send initial state in case the capabilities contain state events.
  - request Matrix event subscriptions
- Composes the correct messages sent to the widget

## Usage

To use this module, two structs are needed:

- The `CapabilitiesProvider`: This trait uses the `acquire_capabilities` method to ask a client which capabilities
  the widget is allowed. An example capability can be: reading events of type `"m.room.message"`.

- The `WidgetDriver` itself, which consists of:
  - The `driver`: Its only public method is `run`, which is used to actually start the widget communication.
  - The message `handle`: This handle has `recv` and `send` methods.
    A JSON message sent from the widget to the client needs to be passed to the handle via `send`.
    Any JSON message the handle emits via `recv` needs to be sent to the widget via `postMessage`.

```rust
#[derive(Clone)]
struct CapProv {}

impl CapabilitiesProvider for CapProv {
    async fn acquire_capabilities(&self, requested_capabilities: Capabilities) -> Capabilities {
        // Only approve capabilities the user has approved interactively
        let approved_capabilities = prompt_user_for_capabilities(requested_capabilities).await;
        approved_capabilities
    }
}

let widget_settings = WidgetSettings {
    widget_id: "String",
    init_on_content_load: true,
    raw_url: "https://Url",
};

// Create the required structs:
let (driver, handle) = WidgetDriver::new(widget_settings);
let cap_provider = CapProv {};

// Set up message routing
let h = handle.clone();
spawn(async move {
    loop {
        let message = my_platform::postmessage_receiver.recv::<String>().await;
        h.send(message).await;
    }
});

spawn(async move {
    while let Some(msg) = handle.recv().await {
        let script = format!(
            "document.getElementById('widget').contentWindow.postMessage({}, '{}');",
            message, url,
        );
        my_platform::eval(&script);
    }
});

spawn(async move {
    let _ = driver.run(room, cap_provider).await;
});
```

## Feature Flags

This module will only be active when used with the `experimental-widgets` feature flag.
