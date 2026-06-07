# Telescreen — Architectural Specification

> Transparent Terminal Session Logging Daemon

| | |
|---|---|
| **Version** | 1.5 |
| **Language** | Rust |
| **Platform** | Linux |
| **Status** | Active — v1.5 |

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [System Architecture](#2-system-architecture)
3. [Component Specifications](#3-component-specifications)
4. [Wire Protocol](#4-wire-protocol)
5. [Configuration](#5-configuration)
6. [CLI Reference](#6-cli-reference)
7. [Data Flow](#7-data-flow)
8. [Installation](#8-installation)
9. [Non-Functional Requirements](#9-non-functional-requirements)
10. [File & Directory Layout](#10-file--directory-layout)
11. [Dependencies](#11-dependencies)
12. [Feature Checklist](#12-feature-checklist)
13. [Known Issues & Future Work](#13-known-issues--future-work)
14. [Bug Fix Log](#14-bug-fix-log)

---

## 1  Executive Summary

Telescreen is a Linux terminal session logging daemon written in Rust. It silently intercepts every interactive shell session on a machine — SSH logins, local terminals, `su`, `sudo -i` — and writes a full record of all input and output to two log formats: a human-readable timestamped text log and a structured JSONL log suited for LLM ingestion or log analytics platforms.

**The user sees no difference.** Their prompt, colours, readline, and interactive programs all work normally.

### Design principles

- **One binary, one config file.** No external shell scripts to maintain.
- **No login config changes.** `/etc/passwd`, `/etc/shells`, and every user's login shell remain untouched.
- **Daemon/client split.** A persistent daemon runs as root; each shell session connects as a lightweight PTY bridge.
- **Graceful degradation.** If the daemon is not running, sessions pass through to the real shell silently.
- **Pure std threads after fork.** No Tokio runtime in any forked child process — a hard invariant.
- **Accurate command logging.** A software `LineEditor` reconstructs the exact command from raw stdin bytes, handling backspace, delete, arrow keys, and cursor movement. The PTY-rendered prompt line is used as a cross-check fallback.

---

## 2  System Architecture

### 2.1  High-Level Overview

```
Boot
 └── telescreen start (or systemd unit)
       └── double-fork → Telescreen Daemon (PID D)
                            ├── /run/telescreen/agent.sock   ← session connections  (0666)
                            └── /run/telescreen/ctrl.sock    ← stop/status/flush    (0600 root-only)

New terminal / SSH session
 └── bash sources /etc/profile.d/telescreen.sh
       └── exec telescreen session
             ├── sends M frame (session metadata: UUID, user, uid, host, tty, shell, cwd, term, rows, cols, remote_ip)
             ├── sends R frame (initial terminal size)
             ├── allocates PTY (size from TIOCGWINSZ)
             ├── forks real $SHELL into PTY slave
             └── bridge_loop (transparent proxy):
                   stdin bytes → PTY master → shell
                   LineEditor tracks keystrokes → on Enter: send I frame
                   PTY rendered line → fallback command verification
                   PTY output → user terminal + O frame to daemon
                   SIGWINCH → TIOCSWINSZ + R frame to daemon
                   shell exits → E frame to daemon

Daemon
 └── session_logger thread (one per session, RecentOutput Arc shared across all)
       ├── Rejects session if first frame ≠ M (→ session_incomplete event)
       └── writer_thread:
             M → [SESSION START] text + session_start JSON
             I → flush pending output → [STDIN] entry
             O → line-buffer → ANSI strip → filter prompts
               → suppress echo line (matches last STDIN + prompt marker)
               → dedup consecutive lines
               → cross-session dedup (RecentOutput hash cache, 50ms window)
               → [OUTPUT] block
             R → resize JSON event
             E → [SESSION END] text + session_end JSON
             Incomplete → [SESSION INCOMPLETE] text + JSON
             new PID → [USER CHANGE] + user_change JSON
```

### 2.2  Module Map

| Module | File | Responsibility |
|---|---|---|
| `cli` | `src/cli.rs` | Subcommand definitions; `session` hidden from user help |
| `config` | `src/config.rs` | `TelescreenConfig`; YAML load; layered search; `write_default()` |
| `daemon` | `src/daemon.rs` | All daemon, bridge, writer, install logic |

Dead code (no longer compiled):
- `src/logger.rs`, `src/process_manager.rs`, `src/stream_interceptor.rs` — remnants of the removed wrap mode.

---

## 3  Component Specifications

### 3.1  Daemon

#### 3.1.1  Double-Fork Protocol

1. `start()` called — no Tokio runtime.
2. **Fork #1:** parent prints banner and returns.
3. Child calls `setsid()`.
4. **Fork #2:** intermediate exits. Grandchild re-parented to PID 1.
5. Grandchild writes PID file (via raw `libc::open`), redirects stdio to `/dev/null`, enters `daemon_main()`.

> **Post-fork rule:** no `?`, no `return Err()`, no Rust unwinding after fork #1.

#### 3.1.2  Socket Security

| Socket | Mode | Access |
|---|---|---|
| `agent.sock` | `0666` | Any user; peer UID logged via `SO_PEERCRED` |
| `ctrl.sock` | `0600` | Root only; UID verified via `SO_PEERCRED` before command execution |
| Socket dir | `0755` | World-traversable |

#### 3.1.3  Session Logger Thread

One thread per agent connection. Enforces protocol:

- **First frame must be `M`.** Any other frame type → `LogMsg::Incomplete` → `[SESSION INCOMPLETE]` log entry.
- **EOF without `E` frame** → `LogMsg::Incomplete { reason: "EOF without E frame" }`.
- **Unknown frame type** → `LogMsg::Incomplete { reason: "unknown frame type 'X'" }`.

Dispatches frames to the writer thread via a bounded `mpsc::sync_channel(8192)`.

A **flush-timer thread** sends `LogMsg::Flush` every 150 ms. The writer flushes pending output lines if they have been idle for >100 ms, and also calls `f.flush()` on both log file handles.

#### 3.1.4  Cross-Session Output Deduplication

`RecentOutput` is an `Arc<Mutex<VecDeque<(Instant, u64)>>>` shared across all `session_logger` threads (allocated once in `daemon_main`). Before writing an OUTPUT block, the writer hashes the content and checks this cache — if an identical hash was recorded within the last 50 ms, the block is suppressed. This eliminates the duplicate output that occurs when a parent shell and a child shell (`sudo -i`) both forward the same PTY bytes to the daemon simultaneously.

#### 3.1.5  Connection Throttling

The daemon limits concurrent sessions to 100 (hard-coded max). Additional connections are rejected with an error on the agent socket. `ACTIVE_SESSIONS` is tracked via an `AtomicUsize` incremented/decremented around each session.

#### 3.1.6  Shutdown Sequence

When a STOP frame is received on the control socket:
1. `SHUTDOWN` `AtomicBool` is set to `true`.
2. A 500 ms grace period allows writer threads to flush pending output.
3. All agent connections are accepted but immediately dropped.
4. PID file is removed, sockets are unlinked, and the daemon exits.

---

### 3.2  Session Bridge (`session` subcommand)

Runs in the **foreground** as the user's shell process. Invoked by `exec` in `/etc/profile.d/telescreen.sh`.

#### 3.2.1  Recursion Guard

Reads `/proc/<ppid>/exe` and `/proc/<ppid>/comm`. If parent is `telescreen`, we are already inside a logged session — `execvp($SHELL)` directly. This avoids the env-var approach which persists into grandchild shells.

#### 3.2.2  Session Startup Sequence

1. Connect to `agent.sock`. On failure: `execvp($SHELL)` silently.
2. Collect `SessionMeta` (UUID, user, uid, host, cwd, term, rows, cols, remote_ip, ppid).
3. Send **M frame** (JSON-serialised `SessionMeta`) — must be first frame.
4. Send **R frame** (initial terminal size).
5. `openpty()` — allocate PTY of matching size.
6. Fork shell into PTY slave.
7. Install `SIGWINCH` handler (resizes PTY via `TIOCSWINSZ`, sends R frame via async-signal-safe `send_frame_raw()`).
8. Enter `bridge_loop()`.
9. On return: send **E frame** (exit status + reason).

#### 3.2.3  LineEditor — Software Stdin Reconstruction

The bridge maintains a `LineEditor` struct that mirrors readline's internal buffer by processing raw stdin bytes character by character:

| Byte(s) | Action |
|---|---|
| `\r` or `\n` | Flush: return current buffer as the submitted command; reset |
| `0x7f` / `0x08` | Backspace: remove char before cursor |
| `ESC [ D` | Cursor left |
| `ESC [ C` | Cursor right |
| `ESC [ 3 ~` | Delete (forward) |
| `ESC [ H` | Home (cursor to start) |
| `ESC [ F` | End (cursor to end) |
| `>= 0x20` | Insert printable char at cursor position |

This correctly reconstructs commands that involve history recall, backspace over mistakes, arrow-key editing, and cursor movement — giving the exact string the shell will execute.

#### 3.2.4  Rendered Prompt Line — Fallback Verification

In parallel, `update_rendered_prompt_line()` tracks the PTY's current display line (the last line the user sees) by processing raw stdin bytes with simple rules: `\r` clears, backspace removes last char, printable chars append. When Enter is pressed, `extract_command_from_prompt_line()` searches this string for the last `$ `, `# `, `% `, or `> ` marker and returns the text after it.

If the rendered prompt extraction produces a non-empty result, it **overrides** the `LineEditor` result. This handles history recall (`↑ Enter`) where readline rewrites the display but the LineEditor has no visibility into the history buffer.

#### 3.2.5  I Frame Transmission

Commands are sent as JSON I frames immediately when Enter is pressed:

```json
{
  "command": "ls -l",
  "method":  "stdin_line_editor",
  "time":    "2026-05-28T18:34:52.792Z",
  "pid":     3974
}
```

The `method` field is `"stdin_line_editor"` (primary) or `"pty_heuristic"` (fallback, from rendered prompt line).

#### 3.2.6  O Frame Transmission

Raw PTY output bytes are forwarded to the user's terminal and simultaneously sent as O frames. Each O frame payload begins with a 4-byte little-endian shell PID (legacy compat shim), followed by the raw bytes.

#### 3.2.7  PTY Bridge Loop Summary

```
select(stdin, master_fd, timeout=50ms)

stdin readable:
  read bytes d
  update_rendered_prompt_line(d)        ← track display state
  write(master_fd, d)                   ← forward to shell
  if process_stdin_bytes(editor, d) → Some(cmd):
    if extract_command_from_prompt_line(rendered) → Some(pty_cmd):
      cmd = pty_cmd                     ← prefer PTY rendering
    send I frame {command: cmd, pid, method, time}

master_fd readable:
  read bytes d
  write(STDOUT_FILENO, d)              ← passthrough to user
  send O frame [pid_le4][d]            ← to daemon

waitpid(WNOHANG) == shell_pid → break
```

---

### 3.3  Writer Thread

#### 3.3.1  Command Echo Suppression

After logging a STDIN entry, `last_stdin` is set to the command string. For each subsequent OUTPUT line, if the line contains the command text AND a prompt marker (`$ `, `# `, etc.), it is identified as the PTY echo of the submitted command and suppressed (once). This prevents the prompt+command redisplay from appearing as output.

#### 3.3.2  Shell-Init Quiet Window

A 300 ms quiet window is applied after `SessionStart` and after each `UserChange` event. OUTPUT frames arriving during this window are discarded. This suppresses readline's terminal initialization sequences (history redraw, terminal state reconstruction) that are emitted when bash/zsh start.

#### 3.3.3  User Change Detection

On every `Entry` frame, if the PID has not been seen before in this session:
1. Read `/proc/<pid>/status` → UID → `nix::unistd::User::from_uid()` → username.
2. Append `(pid, username)` to `pid_chain`.
3. Emit `[USER CHANGE] ubuntu(2843) -> root(6807)` text entry and `user_change` JSON event.
4. Reset the 300 ms quiet window.

#### 3.3.4  Output Block Assembly

```
O frame arrives
  ├── skip if within shell_init_quiet window
  ├── ANSI-strip (if strip_ansi = true)
  ├── line-buffer into out_buf
  └── for each complete \n-terminated line:
        ├── skip if empty after trimming
        ├── skip if is_prompt_line()      ← ends with $/#/>/%  AND contains @/:~
        ├── skip if matches last_stdin echo (prompt marker + command text)
        ├── skip if identical to previous line (consecutive dedup)
        └── push to out_lines

LogMsg::Flush (every 150ms) OR LogMsg::Entry { source: "STDIN" }:
  ├── consecutive-line dedup within out_lines
  ├── join with \n
  ├── check RecentOutput hash cache (cross-session dedup, 50ms window)
  └── write_entry() → text log + JSONL
```

---

### 3.4  Install / Uninstall

#### `telescreen install` (requires sudo)

Writes `/etc/profile.d/telescreen.sh`:
```sh
# managed by telescreen
case $- in *i*) ;; *) return ;; esac
[ -n "$TELESCREEN_SESSION" ] && return
[ -S "/run/telescreen/agent.sock" ] || return
export TELESCREEN_SESSION=1
exec "/usr/bin/telescreen" session
```

Creates `/var/log/telescreen/` and `/run/telescreen/` (`0755`). Writes default config if absent.

#### `telescreen uninstall` (requires sudo)

Removes `/etc/profile.d/telescreen.sh` only.

---

## 4  Wire Protocol

Implements the Terminal Logging Protocol Specification v1.

### 4.1  Frame Format

```
[1-byte type][4-byte BE payload_length][payload bytes]
```

### 4.2  Frame Types

| Type | Direction | Payload | Notes |
|---|---|---|---|
| `M` | bridge→daemon | JSON `SessionMeta` | **Must be first frame**; session rejected otherwise |
| `O` | bridge→daemon | `[4-byte LE pid][raw PTY bytes]` | Raw terminal output |
| `R` | bridge→daemon | JSON `{rows, cols, time?}` | Terminal resize |
| `I` | bridge→daemon | JSON `{command, method, time?, pid}` | Command annotation |
| `E` | bridge→daemon | JSON `{ended_at, reason, exit_status?}` | Session end; closes session |
| `D` | bridge→daemon | JSON `{level, code, message, time?}` | Diagnostic; not logged to session log |

Unknown frame types are treated as protocol errors → `session_incomplete` event.

### 4.3  Session Lifecycle

```
bridge connects to agent.sock
  │
  ├─ M frame  (mandatory first)
  ├─ R frame  (initial size)
  ├─ [O / I / R frames interleaved]
  └─ E frame  (session end, clean)
         OR
     EOF without E  (→ session_incomplete)
```

### 4.4  SessionMeta Fields

```json
{
  "protocol_version": 1,
  "session_id":   "uuid-v4",
  "started_at":   "2026-05-28T18:34:52.792Z",
  "user":         "ubuntu",
  "uid":          1000,
  "host":         "telescreen",
  "shell":        "/bin/bash",
  "cwd":          "/home/ubuntu",
  "term":         "xterm-256color",
  "rows":         24,
  "cols":         80,
  "remote_ip":    "10.0.0.5",
  "ppid":         1234
}
```

---

## 5  Configuration

### 5.1  Config File Search Path (priority order)

1. `--config <path>` CLI flag
2. `$TELESCREEN_CONFIG` env var
3. `~/.config/telescreen/config.yaml`
4. `/etc/telescreen/config.yaml`

### 5.2  Schema

```yaml
pid_file:    /run/telescreen/telescreen.pid
agent_sock:  /run/telescreen/agent.sock
ctrl_sock:   /run/telescreen/ctrl.sock
diag_log:    /var/log/telescreen/daemon.log
output_log:  /var/log/telescreen/session.log
json_log:    /var/log/telescreen/session.jsonl
format:      both          # text | json | both
shell:       /bin/bash     # overridden by /etc/passwd at session time
log_stdin:   true
log_stdout:  true
log_stderr:  true
strip_ansi:  true
```

### 5.3  Log Formats

**Text log:**
```
[2026-05-28 18:34:52.792] [SESSION START] user=ubuntu uid=1000 host=telescreen tty=/dev/pts/3 cwd=/home/ubuntu ip=10.0.0.5
[2026-05-28 18:35:01.100] [STDIN]  [ubuntu] [PID: 3974] ls -l
[2026-05-28 18:35:01.120] [OUTPUT] [ubuntu] [PID: 3974] total 4
drwxrwxr-x 1 ubuntu ubuntu    0 May 26 20:10 build
[2026-05-28 18:35:10.200] [USER CHANGE] ubuntu(3974) -> root(4009)
[2026-05-28 18:35:44.100] [SESSION END] reason=child_exit exit=0
```

**JSONL log:**
```json
{"type":"session_start","session_id":"a1b2-...","user":"ubuntu","uid":1000,...}
{"type":"stdin","session_id":"a1b2-...","username":"ubuntu","process_id":3974,"content":"ls -l"}
{"type":"output","session_id":"a1b2-...","username":"ubuntu","process_id":3974,"content":"total 4\ndrwxrwxr-x ..."}
{"type":"user_change","session_id":"a1b2-...","pid":4009,"user":"root","prev_user":"ubuntu","pid_chain":"ubuntu(3974) -> root(4009)"}
{"type":"session_end","session_id":"a1b2-...","reason":"child_exit","exit_status":0}
{"type":"session_incomplete","session_id":"a1b2-...","reason":"EOF without E frame"}
{"type":"resize","session_id":"a1b2-...","rows":30,"cols":120}
```

---

## 6  CLI Reference

| Subcommand | sudo | Description |
|---|---|---|
| `start` | Yes | Start daemon in background |
| `stop` | Yes | Stop daemon |
| `status` | No | Daemon PID, status, log paths |
| `flush` | No | Flush log buffers |
| `install` | Yes | Write profile.d hook, create dirs, write default config |
| `uninstall` | Yes | Remove profile.d hook |
| `default-config` | No | Print default config YAML |
| `session` | No | *(internal)* PTY bridge — called by profile.d |

---

## 7  Data Flow

### 7.1  Normal Command (`ls -l` with arrow-key editing)

```
User presses ↑ (recalls "ls -la"), then Backspace, then Enter
    │
    ▼ raw bytes: ESC[A ESC[B 0x7f 0x0d
bridge stdin handler:
  update_rendered_prompt_line() → "ubuntu@host:~$ ls -l"
  LineEditor: ↑=noop, ←/→=move, 0x7f=backspace → buf="ls -l"
  0x0d (Enter): process_stdin_bytes returns Some("ls -l")
  extract_command_from_prompt_line("ubuntu@host:~$ ls -l") → "ls -l"
  preferred → cmd = "ls -l"
  send I frame {"command":"ls -l","method":"stdin_line_editor","pid":3974}
    │
    ▼ daemon session_logger
  LogMsg::Entry { source:"STDIN", pid:3974, content:"ls -l" }
    │
    ▼ writer_thread
  flush_output()  ← write any pending output first
  last_stdin = Some("ls -l")
  [STDIN] [ubuntu] [PID: 3974] ls -l
```

### 7.2  User Switch (`sudo -i`)

```
[STDIN]  [ubuntu] [PID: 3974] sudo -i
                                │
                                └── sudo opens new PTY session
[SESSION START] user=root uid=0 ...     ← new bridge connects
[USER CHANGE] ubuntu(3974) -> root(4009)  ← new PID in writer
```

### 7.3  Graceful Degradation

```
profile.d: [ -S /run/telescreen/agent.sock ] → FAILS
           → return (skip silently)
           → user gets normal unlogged shell
```

---

## 8  Installation

### 8.1  Quick Start

```bash
cargo build --release
sudo cp target/release/telescreen /usr/bin/telescreen
sudo telescreen install
sudo telescreen start
# Open a new terminal or SSH session — logged automatically
tail -f /var/log/telescreen/session.log
```

### 8.2  Systemd Unit

```ini
[Unit]
Description=Telescreen terminal session logger
After=network.target

[Service]
Type=forking
PIDFile=/run/telescreen/telescreen.pid
ExecStart=/usr/bin/telescreen start
ExecStop=/usr/bin/telescreen stop
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

### 8.3  What install Touches

| Item | Touched | Notes |
|---|---|---|
| `/etc/profile.d/telescreen.sh` | ✅ Written | Removed by `uninstall` |
| `/var/log/telescreen/` | ✅ Created | `0755` |
| `/run/telescreen/` | ✅ Created at `start` | `0755` |
| `~/.config/telescreen/config.yaml` | ✅ Written | Only if absent |
| `/etc/passwd`, `/etc/shells` | ❌ Never | |
| Any user's `$SHELL` | ❌ Never | |

---

## 9  Non-Functional Requirements

### 9.1  Performance

| Requirement | Target | Mechanism |
|---|---|---|
| Logging overhead | < 5% CPU | `select()` loop; bounded mpsc channel |
| Log write latency | < 150 ms | Flush-timer thread; file `flush()` on every Flush tick |
| Memory per session | < 2 MB | 8 KiB read buffer; LineEditor ≤ 4096 chars |

### 9.2  Reliability

- **Daemon crash:** stale PID detected; removed on next `start`.
- **Bridge crash:** `restore_termios()` runs in exit path.
- **Disk full:** write errors ignored; rate-limited warning, session continues.
- **Protocol error:** `session_incomplete` event logged; no crash.

### 9.3  Security

| Item | Value |
|---|---|
| Agent socket | `0666`; peer UID logged |
| Control socket | `0600`; root-only enforced via `SO_PEERCRED` |
| Log files | `0644` root-owned |
| No network sockets | Unix domain only |
| Recursion guard | `/proc/<ppid>/exe` — not env var |

---

## 10  File & Directory Layout

### 10.1  Source Tree

```
telescreen/
├── Cargo.toml
├── LICENSE
├── ARCHITECTURE.md
├── README.md
├── .gitignore
├── config.example.yaml
└── src/
    ├── main.rs                 plain fn main(); dispatches subcommands
    ├── cli.rs                  subcommands
    ├── config.rs               TelescreenConfig; YAML; write_default()
    ├── daemon.rs               everything: daemon, bridge, writer, install
    │   ├── LineEditor          software stdin reconstruction
    │   ├── bridge_loop()       PTY proxy + command extraction
    │   ├── session_logger()    frame dispatch, protocol enforcement
    │   ├── writer_thread()     log formatting, dedup, user-change
    │   └── install/uninstall   profile.d management
    ├── logger.rs               dead code (wrap-mode remnant, not compiled)
    ├── process_manager.rs      dead code (wrap-mode remnant, not compiled)
    └── stream_interceptor.rs   dead code (wrap-mode remnant, not compiled)
```

### 10.2  Runtime Files

| Path | Mode | Purpose |
|---|---|---|
| `/run/telescreen/telescreen.pid` | 644 | Daemon PID |
| `/run/telescreen/agent.sock` | 666 | Session connections |
| `/run/telescreen/ctrl.sock` | 600 | Control — root only |
| `/var/log/telescreen/daemon.log` | 644 | Daemon diagnostics |
| `/var/log/telescreen/` | 755 | Per-session text logs (directory mode) |
| `/var/log/telescreen/session.jsonl` | 644 | JSONL session log (shared across sessions) |
| `/etc/profile.d/telescreen.sh` | 644 | Profile hook |
| `~/.config/telescreen/config.yaml` | 644 | Per-user config |

---

## 11  Dependencies

| Crate | Version | Features | Purpose |
|---|---|---|---|
| `clap` | 4.x | `derive` | CLI |
| `serde` | 1.x | `derive` | Serialization |
| `serde_json` | 1.x | — | JSONL, frame payloads |
| `serde_yaml` | 0.9 | — | Config file |
| `chrono` | 0.4 | `serde` | UTC timestamps |
| `nix` | 0.29 | `fs, process, signal, term, user` | `openpty()`, `fork()`, `execvp()`, `User::from_uid()` |
| `libc` | 0.2 | — | `select`, `ioctl`, `cfmakeraw`, `SO_PEERCRED` |
| `dirs-next` | 2.x | — | XDG config path |
| `log` + `env_logger` | — | — | Internal diagnostics |
| `uuid` | 1.x | `v4` | Session UUID generation |

---

## 12  Feature Checklist

| Feature | Status | Notes |
|---|---|---|
| Output logging | ✅ | Per-command block; ANSI-stripped; prompts filtered |
| Accurate command logging | ✅ | `LineEditor` software reconstruction + PTY rendered line fallback |
| Arrow-key / history editing | ✅ | `LineEditor` handles ESC sequences, backspace, cursor movement |
| Stdin logging | ✅ | Configurable via `log_stdin` |
| Text log format | ✅ | `[timestamp] [SOURCE] [username] [PID] content` |
| JSONL log format | ✅ | `session_id` + `username` on every record |
| Persistent daemon | ✅ | Double-fork, PID file, systemd-ready |
| Silent interception | ✅ | `/etc/profile.d/` — no login config changes |
| Multi-session support | ✅ | One logger thread per session; shared dedup cache |
| Graceful degradation | ✅ | Daemon absent → plain shell, no error |
| Recursion prevention | ✅ | `/proc/<ppid>/exe` check |
| Terminal resize | ✅ | `SIGWINCH` → `TIOCSWINSZ` + R frame |
| ANSI stripping | ✅ | Configurable; text log only; CSI, OSC, DCS, private markers |
| Output line-buffering | ✅ | Grouped per command; flushed ≤150 ms |
| Session UUID | ✅ | UUID v4 from `/dev/urandom` via `read_exact` |
| Session metadata | ✅ | user, uid, host, tty, cwd, term, rows, cols, remote_ip, ppid |
| User change tracking | ✅ | `[USER CHANGE]` + chain on new PID |
| Username in every entry | ✅ | Text + JSONL |
| Shell-init quiet window | ✅ | 300 ms OUTPUT suppressed after start/user-change |
| Output echo suppression | ✅ | `last_stdin` matched against prompt+command line |
| Consecutive line dedup | ✅ | Within output block |
| Cross-session dedup | ✅ | `RecentOutput` hash cache (50 ms window) |
| Peer credential check | ✅ | `SO_PEERCRED` on both sockets |
| Control socket root-only | ✅ | `0600`; enforced in thread |
| Protocol enforcement | ✅ | M-first; unknown types → `session_incomplete` |
| Session end event | ✅ | E frame → `[SESSION END]` + `session_end` JSON |
| Session incomplete event | ✅ | `[SESSION INCOMPLETE]` + JSON on protocol violations |
| Resize events in JSONL | ✅ | `resize` record on every R frame |
| Shell resolution from passwd | ✅ | `nix::unistd::User::from_uid()` |
| Install / uninstall | ✅ | `sudo telescreen install/uninstall` |
| Default config generation | ✅ | `telescreen default-config` |
| Connection throttling | ✅ | Max 100 concurrent sessions |
| Clean shutdown with flush | ✅ | `SHUTDOWN` flag + 500 ms grace period |
| Async-signal-safe SIGWINCH | ✅ | No `format!()` in handler; stack-allocated JSON |
| Non-ASCII UTF-8 stdin | ✅ | Multi-byte decode instead of `from_utf8_lossy` |
| Disk-full resilience | ✅ | ENOSPC logged (rate-limited 30s), no crash |
| UUID from `/dev/urandom` | ✅ | `read_exact` prevents partial reads |
| No `/etc/passwd` changes | ✅ | Hard requirement |
| Log rotation | ❌ | Planned |
| Redaction rules | ❌ | Planned |
| PAM backend | ❌ | Planned |
| systemd unit bundled | ❌ | Planned |

---

## 13  Known Issues & Future Work

### 13.1  Current Known Issues

- **History recall (`↑ + Enter`):** `LineEditor` has no visibility into readline's history buffer. When the user presses `↑` and hits Enter without editing, the LineEditor buffer may be empty. The PTY rendered prompt line (`extract_command_from_prompt_line`) is the fallback in this case, but it depends on the prompt ending with a standard marker (`$ `, `# `).

- **Custom prompt markers:** Prompts using only Unicode symbols (powerline, starship) without standard suffixes will prevent PTY-rendered line fallback from working. Ensure `PS1` ends with `$ `, `# `, or similar.

- **Output log files root-owned:** When daemon runs as root, log files are `0644 root:root`. Non-root users cannot read them. Recommended: dedicated `telescreen` service user.

### 13.2  Planned Enhancements

1. Dedicated service user (`telescreen:telescreen`)
2. Per-session log files (`session-<uuid>-<user>.jsonl`) — `output_log` field already supports directory mode
3. Log rotation (size + date, configurable retention)
4. Redaction rules (regex pipeline for passwords/tokens)
5. Bundled systemd unit (generated by `telescreen install`)
6. PAM backend (`pam_exec.so` alternative to `profile.d`)
7. zsh / fish / nushell native hooks
8. TLS remote syslog / OpenTelemetry forwarding

---

## 14  Bug Fix Log

| # | Bug | Root Cause | Fix |
|---|---|---|---|
| BF-01 | `nix` `pty`/`poll` features missing | nix 0.29 moved PTY under `term` | `features = ["term"]`; `libc::select()` direct |
| BF-02 | `OwnedFd` ≠ `RawFd` | nix 0.29 `OpenptyResult` fields are `OwnedFd` | `.into_raw_fd()` immediately after `openpty()` |
| BF-03 | Daemon exits after second fork | `#[tokio::main]` before `fork()` | `main()` is plain `fn`; no Tokio in daemon |
| BF-04 | `/dev/null` redirect fails | `std::fs::File` closes fd before `dup2()` | Raw `libc::open()` / `dup2()` / `close()` |
| BF-05 | Daemon SIGABRT on `block_on` | `new_multi_thread()` after `fork()` — UB | Removed Tokio from daemon; `std::thread` only |
| BF-06 | Session logs empty | Shell isolated in PTY with no user connection | Foreground bridge proxies user ↔ PTY |
| BF-07 | Over-escaped backslashes | Content escaped in send path and again in writer | Escaping only in writer; raw bytes in transport |
| BF-08 | `dirs-next = "0.3"` not found | Version jumped to `2.0.0` | `dirs-next = "2"` |
| BF-09 | `$TELESCREEN_SESSION` parse error | Rust 2021 reserves `$identifier` in `format!()` | Profile snippet via `String::push_str()` |
| BF-10 | Non-root cannot write logs | `0644 root:root` files | `0644` with world-readable; dirs `0755` |
| BF-11 | Recursion guard fires on all shells | `export TELESCREEN_SESSION=1` persists into grandchildren | `/proc/<ppid>/exe` check replaces env var |
| BF-12 | Garbled OUTPUT on session start | readline redraws history on bash init | 300 ms shell-init quiet window |
| BF-13 | Commands logged as `wxit`, `llit` | Raw keystroke accumulation ignores readline editing | `LineEditor` software reconstruction + PTY rendered line fallback |
| BF-14 | `\~` invalid Rust escape | Not a valid escape sequence | Literal `~` |
| BF-15 | Output duplicated after `sudo -i` | Parent + child PTY sharing | `RecentOutput` cross-session hash dedup (50 ms window) |
| BF-16 | SESSION START after first STDIN | Two independent session streams | Expected; `session_id` correlates them in JSONL |
| BF-17 | STDIN entries missing; commands in OUTPUT | `extract_cmd_from_echo()` returned None when no prompt marker; `LogMsg::Flush` macro used undeclared variables | `LineEditor` as primary; rendered prompt as fallback; macro fixed to use `recent` Arc |
| BF-18 | `ls -l-i` — extra chars in command | PTY echo buffer accumulated prior command output residue | `LineEditor` is now the authoritative source; PTY line is fallback only |
| BF-19 | SIGWINCH handler unsound | `format!()` in signal handler (async-signal-unsafe) | Stack-allocated `resize_json` + `write_dec` |
| BF-20 | Non-ASCII UTF-8 input corruption | Byte-by-byte `from_utf8_lossy` corrupted multi-byte sequences | `process_stdin_bytes` decodes multi-byte properly |
| BF-21 | `/dev/urandom` partial read | `read()` could return < 16 bytes | `read_exact` |
| BF-22 | `umask(0)` TOCTOU race | Permanent 0 umask after socket creation | Save/restore old umask |
| BF-23 | ENOSPC drops file handle | `safe_write` set file to `None` on ENOSPC | Rate-limited warning; session continues logging |
| BF-24 | Writer thread doesn't cleanly exit on STOP | No mechanism to wake writer from blocking `recv()` | `recv_timeout(150ms)` + `SHUTDOWN` check |
| BF-25 | Concurrent session leak to daemon process | No limit on agent connections | `ACTIVE_SESSIONS` AtomicUsize, max 100 |
| BF-26 | E frame missing signal info | Only recorded `WIFEXITED` | Added `WIFSIGNALED` + signal number |
| BF-27 | `select()` error breaks daemon | Non-EINTR errors treated as fatal | Log error and continue |
| BF-28 | Silent frame drops on `try_send` | `try_send` returned `Full` silently | Changed to blocking `send` |
| BF-29 | `out_buf` line scanning O(n²) | Scanned from start each iteration | Cursor/index-based O(n) scan |
| BF-30 | Full `SessionMeta` clone per entry | writer cloned `SessionMeta` for every frame | Pass `session_id`/`username` as `&str` |
| BF-31 | Manual `/etc/passwd` parsing | Fragile string parsing | `nix::unistd::User::from_uid()` |
| BF-32 | ANSI strip misses complex sequences | Only handled basic `ESC[m` | DCS, APC, PM, SOS, `?`, `>`, `=` after CSI |
| BF-33 | `$SHELL` panic on non-UTF-8 | `var("SHELL")` panics on non-UTF-8 | `var_os()` + `into_string().ok()` |
| BF-34 | JSON status uses `format!` | Fragile JSON construction | `serde_json::json!` macro |
| BF-35 | `MAX_PAYLOAD` frame silently dropped by writer | Oversized O frames silently lost | Log oversized size before dropping |
