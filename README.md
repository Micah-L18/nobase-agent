# NoBase Agent

> ⚠️ **This codebase has moved to a private repository.**
> This public repository is an **older version** of the NoBase Agent and is
> retained for reference only. It is no longer maintained here — active
> development continues privately.

---

The **NoBase Agent** is a lightweight Rust daemon that runs on a managed server
and connects out to the NoBase gateway over a single secure WebSocket. Once
connected, it lets the NoBase platform monitor the host, run commands, manage
files, drive an interactive shell, and inspect Docker — all without the platform
ever opening an inbound connection to the server.

It is the on-host counterpart to the NoBase backend: instead of the backend
SSHing into each box to scrape metrics (the old model — 17 sequential SSH
commands per poll), the agent collects everything locally and streams it up.

## Table of Contents

- [What it does](#what-it-does)
- [Architecture](#architecture)
- [How a connection works](#how-a-connection-works)
- [Metrics pipeline](#metrics-pipeline)
- [Supported RPC methods](#supported-rpc-methods)
- [Self-update](#self-update)
- [Configuration](#configuration)
- [CLI reference](#cli-reference)
- [Installation](#installation)
- [Project layout](#project-layout)
- [Building from source](#building-from-source)

## What it does

The agent provides the NoBase platform with full remote management of a host:

- **System metrics** — CPU, memory, swap, disks, network, GPU, and per-container
  Docker stats, sampled once per second.
- **Command execution** — one-shot commands (`exec`) and long-running streamed
  commands (`exec.stream`).
- **Interactive shell** — a real PTY-backed shell session (`shell.open`),
  resizable and bidirectional, suitable for a web terminal.
- **File system access** — list, read, write, stat, mkdir, delete, rename, and
  search, plus chunked upload/download for large files.
- **Docker introspection** — list containers, live stats, inspect, and stream
  container logs.
- **Self-management** — heartbeats, API-key rotation, and binary self-update.

Crucially, the agent **only makes outbound connections**. It dials the gateway;
the gateway never dials it. This keeps the host's firewall closed to inbound
traffic.

## Architecture

```
┌──────────────┐        WSS (outbound)        ┌──────────────┐
│  NoBase      │ ◀──────────────────────────▶ │  NoBase      │
│  Agent       │   JSON-RPC 2.0 over          │  Gateway     │
│  (this repo) │   a single WebSocket         │              │
└──────┬───────┘                              └──────┬───────┘
       │                                             │
       │ collects locally                            │
       ▼                                             ▼
┌──────────────┐                              ┌──────────────┐
│ SQLite buffer│                              │  NoBase      │
│ metrics.db   │                              │  Backend     │
└──────────────┘                              └──────────────┘
```

The agent is a single Rust binary (`nobase`) built on **Tokio**. It is split
into two crates:

| Crate | Path | Purpose |
|-------|------|---------|
| `nobase` | `src/` | The agent binary itself. |
| `agent-proto` | `proto/` | Shared JSON-RPC + metrics types, compiled into both the agent and the gateway so the wire format stays in lockstep. |

All messages on the wire are **JSON-RPC 2.0** envelopes (`RpcRequest` /
`RpcResponse`, plus notifications) carried as WebSocket text frames.

Internally the agent runs several cooperating Tokio tasks:

- **Connection loop** — establishes and re-establishes the WebSocket, with
  exponential backoff.
- **Metrics collection task** — samples the system every second and writes to
  the local SQLite buffer. This task lives *outside* the connection loop, so
  metrics keep being recorded even while the agent is disconnected.
- **Metrics push task** — drains unsent rows from the buffer and ships them to
  the gateway in batches.
- **Heartbeat task** — sends `agent.heartbeat` notifications on an interval.
- **Sender task** — serialises an internal channel onto the WebSocket.
- **Receive loop** — parses incoming frames and dispatches RPC requests to the
  appropriate handler.
- **Update check** — a one-shot task that checks for a newer binary shortly
  after the connection stabilises.

## How a connection works

1. **Connect.** The agent opens a WebSocket to `connection.gateway_url`.
2. **Register or reconnect.**
   - If no `api_key` is stored yet, it sends `agent.register` with its one-time
     `registration_token` and host details. The gateway responds with a
     permanent `server_id` and `api_key`, which the agent **persists back into
     its config file** (atomic temp-file + rename) and uses on every future
     connection.
   - If an `api_key` already exists, it sends `agent.reconnect` instead.
3. **Run.** Heartbeat, metrics-push, and update-check tasks spin up. The receive
   loop handles incoming RPC requests concurrently (each request is dispatched
   on its own spawned task).
4. **Reconnect on drop.** If the connection fails, the agent retries with
   exponential backoff (1s, doubling, capped at `reconnect_max_delay_secs`,
   default 300s). If a connection stayed healthy for more than 30s before
   dropping, the backoff is reset to 1s — a transient blip shouldn't be treated
   like a cold start.
5. **Fail fast on bad credentials.** If registration fails with an auth-class
   error, the agent exits with code **78** (`EX_CONFIG`). The systemd unit is
   configured with `RestartPreventExitStatus=78`, so a wrong API key stops the
   service cleanly instead of hammering the gateway in a restart loop.

## Metrics pipeline

The metrics path is designed to **never lose data across disconnects**.

```
every 1s:  collect → store in SQLite buffer (metrics.db)
every 5s:  drain unsent rows → batch → push to gateway → mark as sent
on connect: backfill — drain ALL unsent rows in chunks before steady-state
every 5m:  cleanup — drop rows older than retention, prune sent rows
```

- **Collection** runs continuously, independent of connection state. While the
  agent is offline, samples simply accumulate on disk.
- **Backfill** runs once per (re)connection: it streams every previously-unsent
  data point to the gateway in chunks (`backfill_chunk_size`, default 300)
  before normal pushing resumes, so a server that was offline for hours catches
  the dashboard up the moment it reconnects.
- **Retention** is bounded — buffered metrics older than
  `buffer_retention_hours` (default 24) are pruned, and successfully-sent rows
  are cleaned up too, keeping `metrics.db` small.

The buffer is a SQLite database at `/var/lib/nobase/metrics.db` by default. All
SQLite I/O is done on `spawn_blocking` threads so it never stalls the async
runtime.

You can inspect the buffer locally with the `nobase metrics` command, which
prints a sparkline of recent CPU/memory over a chosen window.

## Supported RPC methods

The gateway can invoke any of these on the agent (handled in
[`src/dispatcher.rs`](src/dispatcher.rs)):

| Category | Methods |
|----------|---------|
| Execution | `exec`, `exec.stream` |
| Shell (PTY) | `shell.open`, `shell.data`, `shell.resize`, `shell.close` |
| File system | `fs.list`, `fs.read`, `fs.write`, `fs.stat`, `fs.mkdir`, `fs.delete`, `fs.rename`, `fs.search` |
| File transfer | `fs.upload.start`, `fs.upload.chunk`, `fs.upload.end`, `fs.download` |
| Metrics | `metrics.collect` |
| Docker | `docker.list`, `docker.stats`, `docker.inspect`, `docker.logs` |
| System | `system.info`, `heartbeat` / `ping` |
| Management | `rotate_key`, `self_update` |

Unknown methods return a JSON-RPC `-32601 Method not found` error.

Handler implementations live under [`src/handlers/`](src/handlers/):

| File | Responsibility |
|------|----------------|
| `exec.rs` | One-shot and streamed command execution, `system.info`. |
| `shell.rs` | PTY-backed interactive shell sessions. |
| `fs.rs` | File system operations and chunked transfer. |
| `docker.rs` | Docker container listing, stats, inspect, log streaming. |
| `metrics.rs` | System metrics collection. |

## Self-update

The agent can replace its own binary in place:

- **Triggered automatically** — a one-shot check runs ~5s after each connection
  stabilises.
- **Triggered remotely** — the gateway can call the `self_update` RPC.
- **Triggered manually** — `sudo nobase update`.

The updater fetches the latest binary from `connection.backend_url`, verifies
its checksum, swaps the binary, and restarts the systemd service — which drops
the current connection and brings it back up on the new version.

## Configuration

The agent reads a TOML file, by default `/etc/nobase/agent.toml`. Only the
`[connection]` section is required; everything else has sensible defaults.

```toml
[connection]
gateway_url        = "wss://agent.nobase.dev/agent/ws"
backend_url        = "https://agent.nobase.dev"   # used for self-update
api_key            = ""        # filled in automatically after registration
registration_token = "xxxx"    # one-time token; cleared once registered
server_id          = ""        # filled in automatically after registration

[heartbeat]
interval_secs            = 30
reconnect_max_delay_secs = 300

[metrics]
collection_interval_secs = 1                          # sample rate
push_interval_secs       = 5                          # batch push rate
include_docker           = true
include_gpu              = true
buffer_retention_hours   = 24
buffer_path              = "/var/lib/nobase/metrics.db"
backfill_chunk_size      = 300

[security]
allowed_commands  = []   # empty = unrestricted
allowed_paths     = []
max_file_size_mb  = 100

[logging]
level       = "info"
format      = "text"     # "text" or "json"
max_size_mb = 50
max_files   = 3
```

After a successful first registration, the agent rewrites this file to store
the issued `api_key` and `server_id` and to clear the one-time
`registration_token`.

## CLI reference

```
nobase run [--config PATH]      Run the agent (the mode systemd uses)
nobase start                    Start the systemd service
nobase stop                     Stop the systemd service
nobase restart                  Restart the systemd service
nobase status                   Show systemd service status
nobase logs [-n LINES] [-f]     View agent logs (journalctl); follows by default
nobase enable                   Enable the agent on boot
nobase disable                  Disable the agent on boot
nobase update [--config PATH]   Self-update the binary from the backend
nobase uninstall                Remove binary, config, service, logs, metrics DB
nobase version                  Print version and OS/arch
nobase metrics [DURATION]       Sparkline of buffered metrics (1m/10m/1h/6h/1d/7d)
```

`update` and `uninstall` must be run as root.

## Installation

The agent is installed via the bundled [`install.sh`](install.sh), which
downloads the binary, writes the config, installs a systemd unit, and registers
the host:

```bash
# Interactive (prompts for your API key)
curl -sSL https://agent.nobase.dev/install | sudo bash

# Non-interactive
curl -sSL https://agent.nobase.dev/install | sudo NOBASE_API_KEY=xxxx bash

# Self-hosted / staging override
curl -sSL https://agent.nobase.dev/install | sudo bash -s -- \
  --gateway wss://gw.example.com/agent/ws --backend https://api.example.com
```

Once installed it runs as the `nobase` systemd service and starts on boot.

## Project layout

```
.
├── Cargo.toml            Workspace manifest for the `nobase` binary
├── install.sh            One-line installer (download, configure, register)
├── proto/                agent-proto crate — shared wire types
│   └── src/
│       ├── rpc.rs        JSON-RPC 2.0 envelopes
│       ├── methods.rs    Request/response payload types per method
│       ├── metrics.rs    MetricsPayload and sub-structs
│       └── stream.rs     Streaming notification types
└── src/
    ├── main.rs           CLI entry point and systemd subcommands
    ├── config.rs         TOML config: load, validate, persist credentials
    ├── connection.rs     WebSocket connection loop, reconnect, task wiring
    ├── dispatcher.rs     Routes incoming RPC methods to handlers
    ├── updater.rs        Self-update: download, verify, swap, restart
    ├── metrics_buffer.rs SQLite-backed metrics buffer
    ├── metrics_cli.rs    `nobase metrics` sparkline command
    └── handlers/         Per-feature RPC handlers (exec, shell, fs, docker, metrics)
```

## Building from source

Requires a recent stable Rust toolchain.

```bash
cargo build --release
# binary at target/release/nobase

cargo test          # run the test suite
```

The release binary is statically linked against rustls (no OpenSSL dependency),
so it drops onto most Linux hosts without extra system packages.

---

*NoBase Agent — older public snapshot. Development continues in a private
repository.*
