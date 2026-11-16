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

Prebuilt binaries are published for macOS (Apple silicon and Intel) and Linux
(x86-64 and ARM64). No Rust toolchain is needed.

### Install script

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/edusperoni/mcp-park/releases/latest/download/mcp-park-installer.sh | sh
```

The script detects your platform, downloads the matching archive from the
latest release, verifies its checksum (when `sha256sum` is available), and
installs the binary as
`~/.local/bin/mcp-park`. It also adds `~/.local/bin` to `PATH` by writing a
small env script under `~/.config/mcp-park/` and sourcing it from your shell
profiles (`~/.profile`, `~/.zshrc`, fish `conf.d`). Pass `--no-modify-path` to
skip that:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/edusperoni/mcp-park/releases/latest/download/mcp-park-installer.sh | sh -s -- --no-modify-path
```

### Manual install

These are the same steps the script performs.

1. Pick the archive for your platform:

   | Platform | Archive |
   |---|---|
   | macOS, Apple silicon | `mcp-park-aarch64-apple-darwin.tar.gz` |
   | macOS, Intel | `mcp-park-x86_64-apple-darwin.tar.gz` |
   | Linux, x86-64 | `mcp-park-x86_64-unknown-linux-musl.tar.gz` |
   | Linux, ARM64 | `mcp-park-aarch64-unknown-linux-musl.tar.gz` |

   The Linux binaries are statically linked and do not depend on the system
   C library.

2. Download it and its checksum from the
   [latest release](https://github.com/edusperoni/mcp-park/releases/latest):

   ```sh
   ARCHIVE=mcp-park-aarch64-apple-darwin.tar.gz
   curl -LO "https://github.com/edusperoni/mcp-park/releases/latest/download/$ARCHIVE"
   curl -LO "https://github.com/edusperoni/mcp-park/releases/latest/download/$ARCHIVE.sha256"
   ```

3. Verify the download:

   ```sh
   grep . "$ARCHIVE.sha256" | shasum -a 256 -c    # macOS
   grep . "$ARCHIVE.sha256" | sha256sum -c        # Linux
   ```

4. Unpack it and put the binary somewhere permanent:

   ```sh
   tar -xzf "$ARCHIVE"
   mkdir -p ~/.local/bin
   mv "${ARCHIVE%.tar.gz}/mcp-park" ~/.local/bin/
   ```

5. Check that it runs:

   ```sh
   ~/.local/bin/mcp-park --version
   ```

### From source

With a recent stable [Rust toolchain](https://rustup.rs):

```sh
cargo install --git https://github.com/edusperoni/mcp-park --locked
```

This puts `mcp-park` in `~/.cargo/bin`.

MCP clients may start servers with a minimal `PATH`, so use the absolute path
to the binary in their configuration.

## Usage

Put `mcp-park --` in front of the server command, using the absolute path to
the binary (after the install script: `/Users/you/.local/bin/mcp-park` on
macOS, `/home/you/.local/bin/mcp-park` on Linux):

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
- **Closing the client side.** Requests still in flight when the client closes
  its input are answered, for up to 10 seconds, before the server is stopped.
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
