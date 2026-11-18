# Embedding Prometheus: the client SDKs

Three libraries let a program drive the agent over the daemon's
[client protocol](client-protocol.md): a Rust crate (`crates/pmt-sdk`), a
Python package (`sdk/python`, standard library only) and a TypeScript
package (`sdk/typescript`, Node 22.18 or later, which runs the source as is).
They speak the same versioned protocol as the TUI and the desktop app, and
are tested against a real daemon.

```python
from prometheus_sdk import Client

client = Client.connect()                       # attaches, or starts the daemon
session = client.sessions.create(toolsets=["file"])
for event in session.prompt("list files"):      # tool.call, tool.result, assistant.message...
    print(event.type)
print(session.result.usage)                     # the turn's token and cost totals
```

```ts
import { Client } from "./sdk/typescript/src/index.ts";

const client = await Client.connect();
const session = await client.sessions.create({ toolsets: ["file"] });
for await (const event of session.prompt("list files")) console.log(event.type);
console.log(session.result?.usage);
```

```rust,ignore
let client = pmt_sdk::Client::connect(&pmt_sdk::Connect::default()).await?;
let session = client.create(&pmt_sdk::Create::default().toolsets(&["file"])).await?;
let turn = session.prompt("list files", |event| println!("{}", event.kind)).await?;
```

## Connecting

`connect(home?, profile?)` attaches to the home's daemon, starting it when
none runs (`prometheus daemon ensure`), and gets a single-use ticket for the
profile (the Python and TypeScript packages run `prometheus daemon ticket`;
pass `binary` when `prometheus` is not on `PATH`). Each SDK release declares
the protocol majors it supports (`SUPPORTED_PROTOCOL_MAJORS`; the Python
`pyproject.toml` repeats it under `[tool.prometheus] rpc_protocol`, the
TypeScript `package.json` under `prometheus.rpc_protocol`). A daemon speaking another major is refused with
a protocol-mismatch error (`ProtocolMismatch`, or `rpc.protocol_mismatch`,
exit 76, in Rust) before any ticket or request; a `-32008` answer to
`rpc.hello` is reported the same way.

A ticket comes only from a running daemon; `ensure` starts one first:

```console
$ prometheus daemon ticket  # exits 69
error: the gateway daemon is not running
hint: start it with: prometheus daemon ensure
```

## Sessions

`sessions.create(toolsets?, model?, system_prompt_overlay?, ephemeral?)`
creates a session attached to the connection:

- `toolsets` becomes the session override `tools.enabled_toolsets`;
- `system_prompt_overlay` is sent as a last system block with every model
  call of the session only: it is never part of the frozen prompt, the
  stored history or the trajectory;
- `ephemeral = true` keeps the transcript out of the session store and the
  trajectories: the session is hidden from `sessions list`, writes no
  trajectory and no model-call recording, and its messages are removed from
  the store when the session unloads (when the daemon stops, or at the next
  daemon start after a crash). Its row and usage stay, so its spend counts
  against every budget and shows in `prometheus usage`. While it runs, its
  messages live in the store like any session's (the turn loop reads them
  back), and SQLite may keep fragments of removed rows in free pages until
  it reuses them, as after `sessions delete`.

`session.prompt(text, attachments?)` streams the turn's events (the
trajectory events of `session.event`) and ends with the result of
`turn.finished`: exit reason, final answer and usage totals.
`session.interrupt()` stops the running turn at its next safe point and
`session.history()` returns the messages so far. Approval and clarifying
requests go to the `on_approval` / `on_clarify` callbacks (`approver` /
`clarifier` in Rust); without them the daemon resolves them unanswered (an
approval is denied). An embedded session is an ordinary client session: the
same capability grants, approvals and budgets apply. One client runs one
prompt at a time.
