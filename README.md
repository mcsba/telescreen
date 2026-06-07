# Telescreen

A transparent terminal session logging daemon for Linux.  
Runs in the background, intercepts interactive shell sessions (SSH, local, `su`, `sudo -i`)
via an `/etc/profile.d` hook, and logs all terminal I/O to text + JSONL files.

---

## Features

| Capability | Status |
|---|---|
| Background daemon (double-fork, PTY) | ✅ |
| All session types: SSH, local, su, sudo | ✅ (via profile hook) |
| Human-readable text log (per-session) | ✅ `host-user-timestamp.log` |
| Structured JSON log (shared) | ✅ JSONL |
| Command extraction | ✅ Via `LineEditor` + PTY rendered line |
| User-change tracking | ✅ `sudo`/`su` elevation events |
| Output deduplication | ✅ Cross-session 50ms window |
| ANSI stripping | ✅ CSI, OSC, DCS, private markers |
| Terminal resize handling | ✅ SIGWINCH → R frame |
| Daemon control | `start` / `stop` / `status` / `flush` |

---

## Build

```bash
cargo build --release
# Binary: target/release/telescreen
```

---

## Usage

### Install

```bash
sudo telescreen install
```

Writes `/etc/profile.d/telescreen.sh` and creates log directories.
Every new interactive shell will automatically connect to the daemon.

### Start the daemon

```bash
sudo telescreen start
```

Forks into the background, creates a Unix socket, and waits for
session connections from the profile hook.

### Check status

```bash
telescreen status
```

### Flush log buffers

```bash
telescreen flush
```

### Stop the daemon

```bash
sudo telescreen stop
```

---

## Log Formats

### Text log (per-session)

```
[2025-06-07 12:34:56.789] [SESSION START] user=alice uid=1000 host=box tty=/dev/pts/3 cwd=/home/alice ip=-
[2025-06-07 12:34:57.001] [STDIN] [alice] [PID: 1234] ls -la
[2025-06-07 12:34:57.050] [OUTPUT] [alice] [PID: 1234] total 42
[2025-06-07 12:34:57.051] [OUTPUT] [alice] [PID: 1234] drwxr-xr-x  2 alice alice 4096 Jun  7 12:34 .
```

### JSON log (shared JSONL)

```json
{"protocol_version":1,"session_id":"a1b2c3d4-...","started_at":"2025-06-07T12:34:56.789Z","user":"alice","uid":1000,...}
{"timestamp":"2025-06-07T12:34:57.001Z","type":"session_start","session_id":"a1b2c3d4-...","user":"alice",...}
{"timestamp":"2025-06-07T12:34:57.050Z","type":"stdin","session_id":"a1b2c3d4-...","source":"STDIN","content":"ls -la"}
{"timestamp":"2025-06-07T12:34:57.051Z","type":"output","session_id":"a1b2c3d4-...","source":"OUTPUT","content":"total 42"}
```

---

## Configuration

See `config.example.yaml` for all options. Config is searched in order:

1. `--config` CLI flag
2. `$TELESCREEN_CONFIG` env var
3. `/etc/telescreen/config.yaml`
4. `~/.config/telescreen/config.yaml`

---

## Subcommands

| Command | Description |
|---|---|
| `start` | Start the daemon |
| `stop` | Stop the running daemon |
| `status` | Show daemon status |
| `flush` | Force-flush log buffers |
| `install` | Install profile hook and log dirs |
| `uninstall` | Remove profile hook |
| `session` | (internal) PTY bridge, called by profile hook |

---

## Architecture

```
┌──────────────┐     ┌──────────────────────────────┐
│  CLI (clap)  │     │         Daemon                │
│              │     │                              │
│ start/stop/  │     │  double-fork daemonization   │
│ status/flush │     │  → Unix socket listener      │
│ install/     │     │  → accept bridge connections │
│ uninstall    │     │  → spawn session_logger per   │
└──────────────┘     │    connection                 │
                     │                              │
                     │  ┌────────────────────────┐  │
                     │  │   session_logger        │  │
                     │  │  recv frames → channel  │  │
                     │  │  spawn writer_thread    │  │
                     │  └────────┬───────────────┘  │
                     │           │ LogMsg channel    │
                     │  ┌────────▼───────────────┐  │
                     │  │   writer_thread         │  │
                     │  │  dedup → strip ANSI    │  │
                     │  │  → text log + JSON log │  │
                     │  └────────────────────────┘  │
                     └──────────────────────────────┘

┌──────────────────┐
│  Bridge (per     │
│  shell session)  │
│                  │
│  PTY alloc       │
│  fork + exec     │
│  select() loop   │
│  stdin → master  │
│  master → stdout │
│  wire frames     │
│  → agent socket  │
└──────────────────┘
```

### Key modules

| Module | Responsibility |
|---|---|
| `cli` | Subcommand definitions |
| `config` | YAML config loading |
| `daemon` | Daemon lifecycle, PTY bridge, session logger, writer, control thread |

---

## Daemon Internals

1. **Double-fork** — classic POSIX daemonisation: shell prompt returns instantly, daemon re-parented to PID 1.
2. **Unix sockets** — `agent.sock` accepts bridge connections from profile hook; `ctrl.sock` accepts control commands.
3. **PTY bridge** — each session allocates a pseudo-terminal, forks a shell, and runs a `select()` loop teeing I/O between user and PTY.
4. **Wire protocol** — framed messages (type + length + payload) over Unix socket: M (metadata), O (output), I (stdin command), R (resize), E (end), D (diagnostic).
5. **Writer thread** — receives frames via `mpsc::sync_channel`, deduplicates, strips ANSI, extracts prompt lines, and writes text + JSON logs.

---

## License

MIT
