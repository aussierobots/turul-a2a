# turul-a2a-types

Ergonomic Rust wrappers over the prost-generated A2A Protocol v1.0 types.

Transport- and storage-agnostic — used by both the server framework
([`turul-a2a`](https://crates.io/crates/turul-a2a)) and the client
library ([`turul-a2a-client`](https://crates.io/crates/turul-a2a-client)).

## Highlights

- `Task`, `TaskStatus`, `TaskState` with state-machine enforcement.
- `Message`, `Part`, `Role` for conversational payloads.
- `Artifact` for task outputs (text, JSON data, raw bytes, URL).
- `wire` module: JSON-RPC method constants and SSE event shapes.

All public types are `#[non_exhaustive]` — new proto fields land as
additive changes, not breaking ones. Raw proto access stays available
via `as_proto()` / `as_proto_mut()` / `into_proto()` as an escape
hatch.

## Task lifecycle

```rust
use turul_a2a_types::{Task, TaskState, TaskStatus, Artifact, Part};

let mut task = Task::new("task-abc", TaskStatus::new(TaskState::Submitted))
    .with_context_id("ctx-123");

task.set_status(TaskStatus::new(TaskState::Working));

task.append_artifact(
    Artifact::new("result-id", vec![Part::text("done")])
        .with_name("greeting"),
);

task.complete(); // transitions to TaskState::Completed
```

The state machine rejects illegal transitions: e.g. you cannot move
from `Completed` back to `Working`. `TaskState::is_terminal()` and
`TaskState::can_transition_to(next)` are public so callers can check
before mutating.

## Messages

```rust
use turul_a2a_types::{Message, Part, Role};

let msg = Message::new(
    "msg-1",
    Role::User,
    vec![Part::text("Hello, agent")],
);

assert_eq!(msg.joined_text(), "Hello, agent");
assert_eq!(msg.message_id(), "msg-1");
```

`Part` covers the four A2A part variants:

| Constructor | Variant |
|---|---|
| `Part::text(s)` | inline text |
| `Part::data(json_value)` | structured JSON payload |
| `Part::url(url, media_type)` | reference to external content |
| `Part::raw(bytes, media_type)` | inline binary |

Helpers (`as_text`, `as_data`, `as_url`, `as_raw`, `parse_data::<T>`)
let executors pattern-match on incoming parts without touching proto
types.

## Artifacts

```rust
use turul_a2a_types::{Artifact, Part};

let art = Artifact::new("artifact-id-1", vec![Part::text("hello")])
    .with_name("greeting")
    .with_description("a friendly hello");
```

`Task::push_text_artifact(id, name, body)` is a one-liner shortcut
when you just want a single text artifact.

## Raw proto escape hatch

If you need a field that isn't covered by the wrapper API, drop down
to the proto:

```rust
let proto: &turul_a2a_proto::Task = task.as_proto();
// or to mutate:
let proto_mut: &mut turul_a2a_proto::Task = task.as_proto_mut();
```

Prefer wrapper methods where they exist — the wrappers preserve the
state-machine invariants that raw proto access can violate.

See the [workspace README](../../README.md) for the project overview
and crate map.

## License

Licensed under either [MIT](../../LICENSE-MIT) or
[Apache 2.0](../../LICENSE-APACHE) at your option.
