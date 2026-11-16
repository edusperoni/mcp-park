# mcp-park

[![CI](https://github.com/edusperoni/mcp-park/actions/workflows/ci.yml/badge.svg)](https://github.com/edusperoni/mcp-park/actions/workflows/ci.yml)

A stdio wrapper that keeps an MCP server stopped until a real call needs it.

MCP clients start every configured stdio server at session start and keep it
running for the whole session, used or not. A typical Node-based server idles
at 50–100 MB, so a handful of servers across a few open sessions adds up to
gigabytes of memory for tools that are mostly never called.

`mcp-park` sits between the client and the server:

1. **Session start.** The server is started and the handshake and tool listing
   pass through untouched, so the client always gets a live, current tool list.
2. **Park.** Once that goes quiet (5 seconds by default) the server is stopped.
   Only the wrapper stays, at about 2 MB.
3. **Wake.** The first real call starts the server again, replays the client's
   handshake to it, and forwards the call. From then on the server stays up
   for the rest of the session.

Nothing is cached on disk, so there is nothing to expire or clear: every
session sees the tool list the server reports at that moment.

## Install

Requires a recent stable [Rust toolchain](https://rustup.rs).

```sh
cargo install --git https://github.com/edusperoni/mcp-park --locked
```

This puts `mcp-park` in `~/.cargo/bin`. MCP clients may start servers with a
minimal `PATH`, so prefer the absolute path in their configuration.

## Usage

Put `mcp-park --` in front of the server command:

```json
{
  "mcpServers": {
    "example": {
      "command": "/absolute/path/to/mcp-park",
      "args": ["--", "node", "/path/to/server/dist/index.js"]
    }
  }
}
```

With Claude Code:

```sh
claude mcp add-json example \
  '{"command":"/absolute/path/to/mcp-park","args":["--","node","/path/to/server/dist/index.js"]}'
```

| Setting | Default | Meaning |
|---|---|---|
| `--park-after <seconds>` or `MCP_PARK_AFTER` | `5` | Idle time before the server is stopped |
| `MCP_PARK_STOP_GRACE` | `2` | Seconds to wait at each step of stopping the server |
| `MCP_PARK_LOG` | unset | File to append start, park and wake events to |

## What counts as a real call

These messages are handshake or listing traffic and never keep the server up:

- `server/discover`, `initialize`, `notifications/initialized`,
  `subscriptions/listen`
- `tools/list`, `prompts/list`, `resources/list`, `resources/templates/list`
- `ping` (answered by the wrapper itself until the first real call)
- `notifications/cancelled`, `notifications/roots/list_changed` (dropped while
  parked)

A listing request that arrives while the server is parked starts it, is
answered live, and the server is parked again.

Everything else is a real call, including any method the wrapper does not
recognise. That makes an unknown protocol addition cost laziness, not
correctness: the server simply stays up, as it would without the wrapper.

## Behaviour worth knowing

- **Never parks mid-request.** The server is only stopped when no request is
  outstanding in either direction.
- **Stopping.** The server's input is closed first; if it has not exited after
  the grace period its process group gets SIGTERM, then SIGKILL.
- **Failures are passed on, not papered over.** If the server exits on its
  own, the wrapper exits with the same code and answers nothing on its behalf.
- **Server state is lost on park.** Anything a server remembers between the
  handshake and its first real call is gone when it restarts. After the first
  real call it is never restarted.
- **The first real call is slower.** It waits for the server to start, which
  for a Node-based server is typically a few hundred milliseconds.

`MCP_PARK_LOG` shows what happened and why:

```
2026-11-16 14:07:23 [97952] start #1 pid 97970 (server/discover)
2026-11-16 14:07:29 [97952] parked
2026-11-16 14:07:56 [97952] staying awake: tools/call
2026-11-16 14:07:56 [97952] start #2 pid 98302 (tools/call)
```

A line such as `park blocked by pending tools/list (id 1)` names what is
keeping a server up.

## Building and testing

```sh
cargo build --release   # target/release/mcp-park
cargo test
```

The tests drive the binary with the handshakes Claude Code 2.1.325 sends
(`tests/fixtures/`), against a strict mock server that rejects calls arriving
before its handshake.

## Limits

- Unix only (macOS and Linux).
- Verified against Claude Code 2.1.325, with servers that use the
  `initialize` handshake and servers that use the stateless
  `server/discover` + `subscriptions/listen` one. A client that introduces a
  new setup message will see the server stay up after it (see above); check
  `MCP_PARK_LOG` after client upgrades.
- Each message must be a single line of JSON, as the MCP stdio transport
  requires.
- The server runs in its own process group, so it is outside the terminal's
  foreground group and does not receive a Ctrl-C typed there.
- If the wrapper itself is killed with SIGKILL it cannot stop the server; the
  server then has to exit on its own when its input closes.

## License

[MIT](LICENSE)
