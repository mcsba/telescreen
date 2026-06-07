//! Daemon, session bridge, install/uninstall.
//!
//! Wire protocol (bridge → daemon) — per TERMINAL_LOGGING_SPECIFICATION.md:
//!
//!   [1-byte type][4-byte BE payload_len][payload bytes]
//!
//!   M  session metadata (JSON, must be first frame)
//!   O  PTY output bytes (raw, sent after writing to user stdout)
//!   R  terminal resize (JSON: {rows, cols, time?})
//!   E  session end     (JSON: {ended_at, reason, exit_status?, signal?})
//!   I  command annotation (JSON: {command, method, time?})
//!   D  diagnostic event   (JSON: {level, code, message, time?})
//!
//! Unknown frame types are protocol errors: session is marked incomplete.

use std::ffi::CString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use nix::pty::{openpty, Winsize};
use nix::unistd::{close, dup2, execvp, fork, ForkResult};
use nix::libc::{self, STDERR_FILENO, STDIN_FILENO, STDOUT_FILENO};

use crate::config::{OutputFormat, TelescreenConfig};

// ─────────────────────────────────────────────────────────────────────────────
// Diagnostic log (raw fd — safe after fork, after stdio redirect)
// ─────────────────────────────────────────────────────────────────────────────

fn diag(dfd: RawFd, msg: &[u8]) {
    unsafe {
        libc::write(dfd, msg.as_ptr() as *const libc::c_void, msg.len());
        libc::write(dfd, b"\n" as *const u8 as *const libc::c_void, 1);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Wire protocol
// ─────────────────────────────────────────────────────────────────────────────

/// Send a frame: [type(1)][len_BE(4)][payload].
fn send_frame(sock: RawFd, ftype: u8, payload: &[u8]) {
    let len = (payload.len() as u32).to_be_bytes();
    unsafe {
        libc::write(sock, &ftype             as *const u8 as *const libc::c_void, 1);
        libc::write(sock, len.as_ptr()       as *const libc::c_void, 4);
        libc::write(sock, payload.as_ptr()   as *const libc::c_void, payload.len());
    }
}

/// Send a JSON frame.
fn send_json_frame(sock: RawFd, ftype: u8, value: &serde_json::Value) {
    if let Ok(bytes) = serde_json::to_vec(value) {
        send_frame(sock, ftype, &bytes);
    }
}

/// Read one frame from a blocking File. Returns None on EOF or protocol error.
fn recv_frame(f: &mut fs::File) -> Option<(u8, Vec<u8>)> {
    let mut type_buf = [0u8; 1];
    if !read_exact(f, &mut type_buf) { return None; }

    let mut len_buf = [0u8; 4];
    if !read_exact(f, &mut len_buf) { return None; }
    let plen = u32::from_be_bytes(len_buf) as usize;

    const MAX_PAYLOAD: usize = 4 * 1024 * 1024; // 4 MiB
    if plen > MAX_PAYLOAD { eprintln!("[telescreen] dropped oversized frame: {plen} bytes"); return None; }

    let mut payload = vec![0u8; plen];
    if plen > 0 && !read_exact(f, &mut payload) { return None; }

    Some((type_buf[0], payload))
}

fn read_exact(f: &mut fs::File, buf: &mut [u8]) -> bool {
    let mut off = 0;
    while off < buf.len() {
        match f.read(&mut buf[off..]) {
            Ok(0)  => return false,
            Ok(n)  => off += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    true
}

// ─────────────────────────────────────────────────────────────────────────────
// Session metadata
// ─────────────────────────────────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct SessionMeta {
    protocol_version: u32,
    session_id:   String,
    started_at:   String,
    #[serde(skip_serializing_if = "Option::is_none")] user:      Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] uid:       Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")] host:      Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] shell:     Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] cwd:       Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] term:      Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] rows:      Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")] cols:      Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")] remote_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] ppid:      Option<u32>,
}

fn collect_session_meta(shell: &str, ws: &Winsize) -> SessionMeta {
    let uid  = unsafe { libc::getuid() };
    let ppid = unsafe { libc::getppid() } as u32;
    let user = get_username(uid);
    let host = fs::read_to_string("/etc/hostname").ok()
        .map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let cwd  = std::env::current_dir().ok()
        .map(|p| p.to_string_lossy().into_owned());
    let term = std::env::var("TERM").ok();
    let remote_ip = std::env::var("SSH_CLIENT").ok()
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
        .or_else(|| std::env::var("SSH_CONNECTION").ok()
            .and_then(|s| s.split_whitespace().next().map(str::to_string)));

    SessionMeta {
        protocol_version: 1,
        session_id:  new_uuid(),
        started_at:  chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        user,
        uid: Some(uid),
        host,
        shell: Some(shell.to_string()),
        cwd,
        term,
        rows: Some(ws.ws_row),
        cols: Some(ws.ws_col),
        remote_ip,
        ppid: Some(ppid),
    }
}

// Cross-session output dedup — suppresses identical output from parent+child PTY sharing.
type RecentOutput = Arc<Mutex<std::collections::VecDeque<(Instant, u64)>>>;
fn make_recent_output() -> RecentOutput { Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(64))) }
fn is_duplicate_output(recent: &RecentOutput, content: &[u8]) -> bool {
    use std::hash::{Hash, Hasher}; use std::collections::hash_map::DefaultHasher;
    let mut h = DefaultHasher::new(); content.hash(&mut h); let hash = h.finish();
    let window = Duration::from_millis(50);
    if let Ok(mut q) = recent.lock() {
        while q.front().map(|(t,_)| t.elapsed() > window).unwrap_or(false) { q.pop_front(); }
        if q.iter().any(|(_,h)| *h == hash) { return true; }
        q.push_back((Instant::now(), hash));
    }
    false
}

fn get_username(uid: u32) -> Option<String> {
    nix::unistd::User::from_uid(uid.into()).ok().and_then(|user| user.map(|u| u.name))
}

fn new_uuid() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-\
             {:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],b[1],b[2],b[3],b[4],b[5],b[6],b[7],
        b[8],b[9],b[10],b[11],b[12],b[13],b[14],b[15])
}

fn tty_name(fd: RawFd) -> String {
    let mut buf = vec![0u8; 64];
    let r = unsafe { libc::ttyname_r(fd, buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if r == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else { "?".to_string() }
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal log messages
// ─────────────────────────────────────────────────────────────────────────────

enum LogMsg {
    SessionStart(Box<SessionMeta>),
    Resize { rows: u16, cols: u16 },
    Entry  { source: &'static str, pid: u32, content: Vec<u8> },
    SessionEnd { reason: String, exit_status: Option<i32> },
    Incomplete { reason: String },
    Flush,
    Shutdown,
}

// ─────────────────────────────────────────────────────────────────────────────
// Pid→username helper (used in writer for user-change events)
// ─────────────────────────────────────────────────────────────────────────────

fn pid_username(pid: u32) -> Option<String> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if line.starts_with("Uid:") {
            let uid: u32 = line.split_whitespace().nth(1)?.parse().ok()?;
            return get_username(uid);
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// PUBLIC: start daemon
// ─────────────────────────────────────────────────────────────────────────────

pub fn start(cfg: &TelescreenConfig) -> Result<(), String> {
    if let Some(pid) = read_pid_file(&cfg.pid_file) {
        if process_is_alive(pid) {
            return Err(format!("Daemon already running (PID {pid}). Use `telescreen stop`."));
        }
        remove_pid_file(&cfg.pid_file);
    }

    for path in [&cfg.output_log, &cfg.json_log, &cfg.diag_log,
                 &cfg.pid_file, &cfg.agent_sock, &cfg.ctrl_sock] {
        if let Some(dir) = std::path::Path::new(path).parent() {
            let _ = fs::create_dir_all(dir);
        }
    }

    let dfd: RawFd = open_append_nofollow(&cfg.diag_log)
        .map_err(|e| format!("Cannot open diag log {}: {e}", cfg.diag_log))?
        .into_raw_fd();
    //EZZZ
    unsafe {
        let flags = libc::fcntl(dfd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(dfd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
    
    let _ = fs::set_permissions(&cfg.diag_log, fs::Permissions::from_mode(0o644));

    let cfg = cfg.clone();
    match unsafe { fork() }.map_err(|e| format!("fork: {e}"))? {
        ForkResult::Parent { child } => {
            println!("[telescreen] Daemon started (PID {child}).");
            println!("[telescreen] Text log : {}", cfg.output_log);
            println!("[telescreen] JSON log : {}", cfg.json_log);
            unsafe { libc::close(dfd) };
            return Ok(());
        }
        ForkResult::Child => {}
    }

    unsafe { libc::setsid() };
    match unsafe { fork() } {
        Ok(ForkResult::Parent { .. }) => unsafe { libc::_exit(0) },
        Ok(ForkResult::Child) => {}
        Err(_) => unsafe { libc::_exit(1) },
    }

    unsafe {
        libc::signal(libc::SIGHUP,  libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGCHLD, libc::SIG_DFL);
    }

    let my_pid = unsafe { libc::getpid() } as u32;
    write_pid_file_raw(&cfg.pid_file, my_pid);
    diag(dfd, b"[diag] grandchild running");
    redirect_stdio_to_devnull();
    daemon_main(dfd, cfg);
    unsafe { libc::close(dfd); libc::_exit(0) };
}

// ─────────────────────────────────────────────────────────────────────────────
// Daemon main loop
// ─────────────────────────────────────────────────────────────────────────────

fn daemon_main(dfd: RawFd, cfg: TelescreenConfig) {
    for sock in [&cfg.agent_sock, &cfg.ctrl_sock] {
        if let Some(dir) = std::path::Path::new(sock).parent() {
            let _ = fs::create_dir_all(dir);
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        }
    }
    let old_umask = unsafe { libc::umask(0o000) };

    let _ = fs::remove_file(&cfg.agent_sock);
    let agent = match UnixListener::bind(&cfg.agent_sock) {
        Ok(l) => {
            let _ = fs::set_permissions(&cfg.agent_sock, fs::Permissions::from_mode(0o666));
            diag(dfd, b"[diag] agent socket ready");
            l
        }
        Err(e) => {
            let m = format!("[diag] FATAL: agent socket: {e}");
            diag(dfd, m.as_bytes());
            return;
        }
    };

    let _ = fs::remove_file(&cfg.ctrl_sock);
    if let Ok(ctrl) = UnixListener::bind(&cfg.ctrl_sock) {
        let _ = fs::set_permissions(&cfg.ctrl_sock, fs::Permissions::from_mode(0o600));
        let pf  = cfg.pid_file.clone();
        let out = cfg.output_log.clone();
        let jsn = cfg.json_log.clone();
        std::thread::spawn(move || control_thread(ctrl, pf, out, jsn));
        diag(dfd, b"[diag] control socket ready (0600)");
    }

    unsafe { libc::umask(old_umask); }

    let recent_out: RecentOutput = make_recent_output();
    for stream in agent.incoming() {
        if let Ok(conn) = stream {
            if ACTIVE_SESSIONS.load(Ordering::Relaxed) >= MAX_SESSIONS {
                diag(dfd, b"[diag] max sessions reached, dropping connection");
                continue;
            }
            ACTIVE_SESSIONS.fetch_add(1, Ordering::Relaxed);
            let peer_uid = get_peer_uid(&conn);
            let m = format!("[diag] session connect from uid={peer_uid:?}");
            diag(dfd, m.as_bytes());

            let out   = cfg.output_log.clone();
            let jsn   = cfg.json_log.clone();
            let fmt   = cfg.format.clone();
            let strip = cfg.strip_ansi;
            let rc = Arc::clone(&recent_out);
            std::thread::spawn(move || {
                session_logger(conn, out, jsn, fmt, strip, rc);
                ACTIVE_SESSIONS.fetch_sub(1, Ordering::Relaxed);
            });
        }
    }
}

fn get_peer_uid(conn: &UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(conn.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED,
        &mut cred as *mut _ as *mut libc::c_void, &mut len) };
    if r == 0 { Some(cred.uid) } else { None }
}

// ─────────────────────────────────────────────────────────────────────────────
// Session logger — one thread per connected bridge
// ─────────────────────────────────────────────────────────────────────────────

fn session_logger(conn: UnixStream, output_log: String, json_log: String,
                  format: OutputFormat, strip: bool, recent: RecentOutput) {
    let (tx, rx) = mpsc::sync_channel::<LogMsg>(8192);

    std::thread::spawn(move || writer_thread(rx, output_log, json_log, format, strip, recent));

    // Flush timer: 150 ms per spec.
    let tx_flush = tx.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(150));
            if SHUTDOWN.load(Ordering::Relaxed) { break; }
            if tx_flush.send(LogMsg::Flush).is_err() { break; }
        }
    });

    let fd = conn.into_raw_fd();
    let mut f: fs::File = unsafe { fs::File::from_raw_fd(fd) };

    // The first frame MUST be M (session metadata). Per spec: reject otherwise.
    let first = recv_frame(&mut f);
    match first {
        Some((b'M', payload)) => {
            match serde_json::from_slice::<SessionMeta>(&payload) {
                Ok(meta) => { let _ = tx.send(LogMsg::SessionStart(Box::new(meta))); }
                Err(_)   => {
                    let _ = tx.send(LogMsg::Incomplete {
                        reason: "invalid metadata JSON".into() });
                    return;
                }
            }
        }
        Some((ft, _)) => {
            let _ = tx.send(LogMsg::Incomplete {
                reason: format!("first frame was '{}' not 'M'", ft as char) });
            return;
        }
        None => {
            let _ = tx.send(LogMsg::Incomplete { reason: "EOF before metadata".into() });
            return;
        }
    }

    // The shell PID is not in the frame header anymore — the bridge embeds it
    // in I-frame JSON payloads. For O frames we track it via pid_chain in the writer.
    // We need a way to pass the PID for OUTPUT frames; bridge embeds it as leading
    // 4 bytes in the payload for O frames (legacy compat shim, see bridge).
    loop {
        match recv_frame(&mut f) {
            None => {
                // EOF without E frame → incomplete session.
                let _ = tx.send(LogMsg::Incomplete { reason: "EOF without E frame".into() });
                break;
            }
            Some((b'O', payload)) => {
                // O payload: [4-byte LE pid][raw bytes]
                if payload.len() < 4 { continue; }
                let pid = u32::from_le_bytes([payload[0],payload[1],payload[2],payload[3]]);
                let content = payload[4..].to_vec();
                let _ = tx.send(LogMsg::Entry { source: "OUTPUT", pid, content });
            }
            Some((b'I', payload)) => {
                // I payload: JSON {command, method, time?, pid?}
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&payload) {
                    let cmd = v["command"].as_str().unwrap_or("").to_string();
                    let pid = v["pid"].as_u64().unwrap_or(0) as u32;
                    if !cmd.is_empty() {
                        let _ = tx.send(LogMsg::Entry {
                            source: "STDIN", pid, content: cmd.into_bytes() });
                    }
                }
            }
            Some((b'R', payload)) => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&payload) {
                    let rows = v["rows"].as_u64().unwrap_or(24) as u16;
                    let cols = v["cols"].as_u64().unwrap_or(80) as u16;
                    let _ = tx.send(LogMsg::Resize { rows, cols });
                }
            }
            Some((b'E', payload)) => {
                let (reason, exit_status) =
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&payload) {
                        (v["reason"].as_str().unwrap_or("child_exit").to_string(),
                         v["exit_status"].as_i64().map(|x| x as i32))
                    } else { ("child_exit".into(), None) };
                let _ = tx.send(LogMsg::SessionEnd { reason, exit_status });
                break;
            }
            Some((b'D', _payload)) => {
                // Diagnostic — logged internally, not to session log.
            }
            Some((ft, _)) => {
                // Unknown frame type — protocol error per spec.
                let _ = tx.send(LogMsg::Incomplete {
                    reason: format!("unknown frame type '{}'", ft as char) });
                break;
            }
        }
    }

    let _ = tx.send(LogMsg::Shutdown);
}

// ─────────────────────────────────────────────────────────────────────────────
// PUBLIC: run_session — foreground PTY bridge
// ─────────────────────────────────────────────────────────────────────────────

fn is_child_of_telescreen() -> bool {
    let ppid = unsafe { libc::getppid() };
    if let Ok(p) = fs::read_link(format!("/proc/{ppid}/exe")) {
        if p.file_name().and_then(|n| n.to_str()) == Some("telescreen") { return true; }
    }
    if let Ok(c) = fs::read_to_string(format!("/proc/{ppid}/comm")) {
        if c.trim() == "telescreen" { return true; }
    }
    false
}

pub fn run_session(cfg: &TelescreenConfig) -> Result<(), String> {
    if is_child_of_telescreen() {
        let shell_c = CString::new(cfg.shell.as_str()).unwrap();
        let _ = execvp(&shell_c, &[shell_c.as_c_str()]);
        return Ok(());
    }

    let shell = resolve_shell(&cfg.shell);

    let sock_fd: RawFd = match UnixStream::connect(&cfg.agent_sock) {
        Ok(s) => s.into_raw_fd(),
        Err(_) => {
            let shell_c = CString::new(shell.as_str()).unwrap();
            let _ = execvp(&shell_c, &[shell_c.as_c_str()]);
            return Ok(());
        }
    };

    std::env::set_var("SHELL", &shell);
    std::env::remove_var("TELESCREEN_SESSION");

    let ws = get_winsize(STDIN_FILENO)
        .unwrap_or(Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 });

    // Send M frame (session metadata) — must be first.
    let meta = collect_session_meta(&shell, &ws);
    if let Ok(bytes) = serde_json::to_vec(&meta) {
        send_frame(sock_fd, b'M', &bytes);
    }

    // Send initial R frame (terminal size).
    let resize_payload = serde_json::json!({
        "rows": ws.ws_row, "cols": ws.ws_col,
        "time": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
    });
    send_json_frame(sock_fd, b'R', &resize_payload);

    let pty = openpty(Some(&ws), None).map_err(|e| format!("openpty: {e}"))?;
    let master_fd: RawFd = pty.master.into_raw_fd();
    let slave_fd:  RawFd = pty.slave.into_raw_fd();

    let shell_c = CString::new(shell.as_str()).unwrap();
    let shell_pid: libc::pid_t = match unsafe { fork() }.map_err(|e| format!("fork: {e}"))? {
        ForkResult::Child => {
            let _ = close(master_fd);
            let _ = close(sock_fd);
            unsafe { libc::setsid(); libc::ioctl(slave_fd, libc::TIOCSCTTY, 0); }
            let _ = dup2(slave_fd, STDIN_FILENO);
            let _ = dup2(slave_fd, STDOUT_FILENO);
            let _ = dup2(slave_fd, STDERR_FILENO);
            if slave_fd > 2 { let _ = close(slave_fd); }
            let _ = execvp(&shell_c, &[shell_c.as_c_str()]);
            unsafe { libc::_exit(1) };
        }
        ForkResult::Parent { child } => { let _ = close(slave_fd); child.as_raw() }
    };

    // SIGWINCH: resize PTY and send R frame.
    unsafe {
        MASTER_FD.store(master_fd, Ordering::Relaxed);
        SOCK_FD.store(sock_fd, Ordering::Relaxed);
        extern "C" fn handle_winch(_: libc::c_int) {
            unsafe {
                let mfd = MASTER_FD.load(Ordering::Relaxed);
                let sfd = SOCK_FD.load(Ordering::Relaxed);
                if mfd < 0 || sfd < 0 { return; }
                let mut ws: libc::winsize = std::mem::zeroed();
                if libc::ioctl(STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 {
                    libc::ioctl(mfd, libc::TIOCSWINSZ, &ws);
                    let mut payload = [0u8; 32];
                    let len = resize_json(ws.ws_row, ws.ws_col, &mut payload);
                    send_frame_raw(sfd, b'R', &payload[..len]);
                }
            }
        }
        libc::signal(libc::SIGWINCH, handle_winch as *const () as libc::sighandler_t);
    }

    let saved = set_raw_mode(STDIN_FILENO);
    let (exit_status, signal) = bridge_loop(master_fd, sock_fd, shell_pid, cfg.log_stdin, cfg.log_stdout);
    restore_termios(STDIN_FILENO, &saved);

    // Send E frame (session end).
    let mut end_payload = serde_json::json!({
        "ended_at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        "reason": if signal.is_some() { "child_signal" } else { "child_exit" },
        "exit_status": exit_status,
    });
    if let Some(sig) = signal {
        end_payload["signal"] = serde_json::Value::Number(sig.into());
    }
    send_json_frame(sock_fd, b'E', &end_payload);

    unsafe { libc::close(master_fd); libc::close(sock_fd); }
    Ok(())
}

static MASTER_FD: AtomicI32 = AtomicI32::new(-1);
static SOCK_FD:   AtomicI32 = AtomicI32::new(-1);
static SHUTDOWN:  AtomicBool = AtomicBool::new(false);
static ACTIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);
const MAX_SESSIONS: usize = 100;

// Async-signal-safe JSON formatter for R frame (no heap allocation).
fn resize_json(rows: u16, cols: u16, buf: &mut [u8; 32]) -> usize {
    buf[0] = b'{';
    buf[1..6].copy_from_slice(b"\"rows");
    buf[6] = b':';
    let mut pos = 7;
    pos += write_dec(rows as u32, &mut buf[pos..]);
    buf[pos..pos + 7].copy_from_slice(b",\"cols\"");
    pos += 7;
    buf[pos] = b':';
    pos += 1;
    pos += write_dec(cols as u32, &mut buf[pos..]);
    buf[pos] = b'}';
    pos + 1
}

fn write_dec(mut n: u32, buf: &mut [u8]) -> usize {
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 10];
    let mut i = 0;
    while n > 0 {
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    let mut j = 0;
    while i > 0 {
        i -= 1;
        buf[j] = tmp[i];
        j += 1;
    }
    j
}

unsafe fn send_frame_raw(sock: RawFd, ftype: u8, payload: &[u8]) {
    let len = (payload.len() as u32).to_be_bytes();
    libc::write(sock, &ftype as *const u8 as *const libc::c_void, 1);
    libc::write(sock, len.as_ptr() as *const libc::c_void, 4);
    libc::write(sock, payload.as_ptr() as *const libc::c_void, payload.len());
}

fn resolve_shell(config_shell: &str) -> String {
    let uid = unsafe { libc::getuid() };
    nix::unistd::User::from_uid(uid.into()).ok()
        .and_then(|u| u)
        .filter(|u| !u.shell.as_os_str().is_empty() && u.shell != std::path::Path::new("/usr/bin/telescreen"))
        .map(|u| u.shell.to_string_lossy().into_owned())
        .unwrap_or_else(|| config_shell.to_string())
}


#[derive(Default)]
struct LineEditor {
    buf: Vec<char>,
    cursor: usize,
}

impl LineEditor {
    fn insert_text(&mut self, s: &str) {
        for ch in s.chars() {
            self.buf.insert(self.cursor, ch);
            self.cursor += 1;
        }
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.buf.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.buf.len() {
            self.buf.remove(self.cursor);
        }
    }

    fn left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    fn right(&mut self) {
        if self.cursor < self.buf.len() {
            self.cursor += 1;
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
    }

    fn line(&self) -> String {
        self.buf.iter().collect()
    }
}

fn process_stdin_bytes(editor: &mut LineEditor, data: &[u8]) -> Option<String> {
    let mut i = 0;

    while i < data.len() {
        match data[i] {
            b'\r' | b'\n' => {
                let cmd = editor.line();
                editor.clear();
                return Some(cmd);
            }

            0x7f | 0x08 => {
                editor.backspace();
                i += 1;
            }

            0x1b => {
                if i + 2 < data.len() && data[i + 1] == b'[' {
                    match data[i + 2] {
                        b'D' => editor.left(),
                        b'C' => editor.right(),
                        b'3' => {
                            if i + 3 < data.len() && data[i + 3] == b'~' {
                                editor.delete();
                                i += 1;
                            }
                        }
                        b'H' => editor.cursor = 0,
                        b'F' => editor.cursor = editor.buf.len(),
                        _ => {}
                    }
                    i += 3;
                } else {
                    i += 1;
                }
            }

            b if b >= 0x20 && b < 0x80 => {
                let s = std::str::from_utf8(&data[i..i + 1]).unwrap();
                editor.insert_text(s);
                i += 1;
            }

            b if b >= 0xC0 && b <= 0xF4 => {
                let seq_len = if b & 0xE0 == 0xC0 { 2 }
                            else if b & 0xF0 == 0xE0 { 3 }
                            else { 4 };
                if i + seq_len <= data.len() {
                    if let Ok(s) = std::str::from_utf8(&data[i..i + seq_len]) {
                        editor.insert_text(s);
                        i += seq_len;
                    } else {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }

            _ => {
                i += 1;
            }
        }
    }

    None
}


// ─────────────────────────────────────────────────────────────────────────────
// PTY bridge loop — returns shell exit status
// ─────────────────────────────────────────────────────────────────────────────

fn bridge_loop(master_fd: RawFd, sock_fd: RawFd, shell_pid: libc::pid_t,
               log_in: bool, log_out: bool) -> (i32, Option<i32>) {
    let mut buf     = [0u8; 8192];
    let pid_u32     = shell_pid as u32;
    let max_fd      = master_fd.max(STDIN_FILENO) + 1;
    let mut rendered_prompt_line = String::new();
    let mut editor = LineEditor::default();
    let mut exit_status = 0i32;
    let mut signal: Option<i32> = None;

    loop {
        let (si, mi) = unsafe {
            let mut rfds: libc::fd_set = std::mem::zeroed();
            libc::FD_SET(STDIN_FILENO, &mut rfds);
            libc::FD_SET(master_fd,    &mut rfds);
            let mut tv = libc::timeval { tv_sec: 0, tv_usec: 50_000 };
            let r = libc::select(max_fd, &mut rfds, std::ptr::null_mut(),
                                 std::ptr::null_mut(), &mut tv);
            if r < 0 {
                let e = nix::errno::Errno::last();
                if e == nix::errno::Errno::EINTR { (false, false) } else {
                    eprintln!("[telescreen] select() error: {:?}, continuing", e);
                    (false, false)
                }
            } else {
                (libc::FD_ISSET(STDIN_FILENO, &rfds), libc::FD_ISSET(master_fd, &rfds))
            }
        };

        if si {
            let n = unsafe {
                libc::read(STDIN_FILENO, buf.as_mut_ptr() as *mut _, buf.len())
            };

            match n {
                n if n > 0 => {
                    let d = &buf[..n as usize];
                    //EZZZZZZZZZZZ
                    update_rendered_prompt_line(&mut rendered_prompt_line, d);

                    unsafe {
                        libc::write(master_fd, d.as_ptr() as *const _, d.len());
                    }

                    if log_in {
                        if let Some(mut cmd) = process_stdin_bytes(&mut editor, d) {

                            // Prefer PTY-rendered line when available.
                            // This fixes:
                            //   ↑ + Enter
                            //   readline redraws
                            //   history recall
                            //   Ctrl+A edits
                            if let Some(pty_cmd) = extract_command_from_prompt_line(&rendered_prompt_line) {
                                if !pty_cmd.is_empty() {
                                    cmd = pty_cmd;
                                }
                            }

                            let cmd = cmd.trim_end().to_string();

                            if !cmd.is_empty() {
                                let payload = serde_json::json!({
                                    "command": cmd,
                                    "method": "stdin_line_editor",
                                    "time": chrono::Utc::now()
                                        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                                        .to_string(),
                                    "pid": pid_u32,
                                });

                                if let Ok(bytes) = serde_json::to_vec(&payload) {
                                    send_frame(sock_fd, b'I', &bytes);
                                }
                            }
                        }
                    }
                }

                _ => break,
            }
        }

        if mi {
            let n = unsafe { libc::read(master_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            match n {
                0 => break,
                n if n < 0 => {
                    let e = nix::errno::Errno::last();
                    if e == nix::errno::Errno::EAGAIN || e == nix::errno::Errno::EWOULDBLOCK {
                        continue;
                    }
                    break;
                }
                n => {
                    let d = &buf[..n as usize];
                    unsafe { libc::write(STDOUT_FILENO, d.as_ptr() as *const _, d.len()); }
                    
                    
                    if log_out {
                        let pid_b = pid_u32.to_le_bytes();
                        let mut frame_data = Vec::with_capacity(4 + d.len());
                        frame_data.extend_from_slice(&pid_b);
                        frame_data.extend_from_slice(d);
                        send_frame(sock_fd, b'O', &frame_data);
                    }
                    
                    }

                }
            }
        let mut st = 0i32;
        let r = unsafe { libc::waitpid(shell_pid, &mut st, libc::WNOHANG) };
        if r == shell_pid {
            (exit_status, signal) = if libc::WIFEXITED(st) {
                (libc::WEXITSTATUS(st), None)
            } else if libc::WIFSIGNALED(st) {
                (128 + libc::WTERMSIG(st), Some(libc::WTERMSIG(st)))
            } else {
                (1, None)
            };
            break;
        }
    }
    (exit_status, signal)
}

fn update_rendered_prompt_line(line: &mut String, data: &[u8]) {
    if let Ok(s) = std::str::from_utf8(data) {
        for c in s.chars() {
            match c {
                '\r' | '\n' => line.clear(),
                '\x08' | '\x7f' => { line.pop(); },
                c if c >= ' ' && c != '\x7f' => line.push(c),
                _ => {}
            }
        }
    }

    // Keep bounded
    if line.len() > 4096 {
        let keep = line.len() - 4096;
        line.drain(..keep);
    }
}

fn extract_command_from_prompt_line(line: &str) -> Option<String> {
    let markers = ["$ ", "# ", "% ", "> "];

    let best = markers.iter()
        .filter_map(|m| line.rfind(m).map(|p| (p, m.len())))
        .max_by_key(|(p, _)| *p);

    if let Some((pos, len)) = best {
        let cmd = line[pos + len..].trim_end();

        if !cmd.is_empty() {
            return Some(cmd.to_string());
        }
    }

    None
}





// Strip ANSI for command extraction only (not for output frames).
fn strip_ansi_bytes(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            0x1b => {
                i += 1;
                if i < input.len() {
                    match input[i] {
                        b'[' => {
                            i += 1;
                            // Handle private mode markers like ? > =
                            while i < input.len() && (input[i] == b'?' || input[i] == b'>' || input[i] == b'=') { i += 1; }
                            while i < input.len() && !(0x40..=0x7e).contains(&input[i]) { i += 1; }
                            if i < input.len() { i += 1; }
                        }
                        b']' => {
                            i += 1;
                            // OSC: consume until BEL (0x07) or ST (ESC \)
                            while i < input.len() && input[i] != 0x07 && input[i] != 0x1b { i += 1; }
                            if i < input.len() {
                                if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b'\\' { i += 2; }
                                else { i += 1; }
                            }
                        }
                        b'P' | b'_' | b'^' | b'X' => {
                            // DCS (P), APC (_), PM (^), SOS (X): consume until ST (ESC \)
                            i += 1;
                            while i < input.len() && input[i] != 0x1b { i += 1; }
                            if i < input.len() { i += 1; }
                            if i < input.len() && input[i] == b'\\' { i += 1; }
                        }
                        _ => { i += 1; }
                    }
                }
            }
            b'\r' => { i += 1; }
            b => { out.push(b); i += 1; }
        }
    }
    out
}


// ─────────────────────────────────────────────────────────────────────────────
// Writer thread
// ─────────────────────────────────────────────────────────────────────────────

fn writer_thread(rx: mpsc::Receiver<LogMsg>, output_log: String,
                 json_log: String, format: OutputFormat, strip: bool,
                 recent: RecentOutput) {

    let mut tf: Option<fs::File> = None;

    let output_path = std::path::Path::new(&output_log);
    let text_log_dir = if output_path.extension().is_some() {
        output_path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| std::path::PathBuf::from("."))
    } else {
        output_path.to_path_buf()
    };
    let mut jf: Option<fs::File> = if format.write_json() {
        let f = open_append_nofollow(&json_log).ok();
        if f.is_some() { let _ = fs::set_permissions(&json_log, fs::Permissions::from_mode(0o644)); }
        f
    } else { None };

    let mut out_buf:   Vec<u8>    = Vec::new();
    let mut out_start: usize      = 0;
    let mut out_lines: Vec<String> = Vec::new();
    let mut out_pid:   u32        = 0;
    let mut meta:      Option<SessionMeta> = None;
    let mut last_stdin: Option<String> = None;
    let mut last_output = Instant::now();
    // 300 ms quiet window after session/user-change start — suppresses readline init noise.
    let mut session_start_time = Instant::now();
    let shell_init_quiet = Duration::from_millis(300);
    // PID chain for user-change tracking.
    let mut pid_chain: Vec<(u32, String)> = Vec::new();

    macro_rules! flush_output {
        () => {
            if !out_lines.is_empty() {
                let mut deduped: Vec<String> = Vec::new();
                let mut prev_line: Option<&String> = None;
                for l in &out_lines {
                    if prev_line != Some(l) { deduped.push(l.clone()); }
                    prev_line = Some(l);
                }
                out_lines.clear();
                if !deduped.is_empty() {
                    let block = deduped.join("\n");
                    if !is_duplicate_output(&recent, block.as_bytes()) {
                        write_entry(&mut tf, &mut jf, "OUTPUT", out_pid,
                                    block.as_bytes(),
                                    meta.as_ref().map(|m| m.session_id.as_str()),
                                    meta.as_ref().and_then(|m| m.user.as_deref()));
                    }
                }
            }
        }
    }

    loop {
        let msg = match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(msg) => msg,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if SHUTDOWN.load(Ordering::Relaxed) { flush_output!(); break; }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match msg {
            LogMsg::Shutdown => { flush_output!(); break; }

            LogMsg::Incomplete { reason } => {
                flush_output!();
                let now = chrono::Utc::now();
                    let mut rec = serde_json::json!({
                        "timestamp": now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
                        "type": "session_incomplete",
                        "reason": reason,
                    });
                    if let Some(m) = &meta { rec["session_id"] = m.session_id.clone().into(); }
                    let mut line = rec.to_string(); line.push('\n');
                    safe_write(&mut jf, line.as_bytes());
                
                let line = format!(
                    "[{}] [SESSION INCOMPLETE] {reason}\n",
                    now.format("%Y-%m-%d %H:%M:%S%.3f")
                );

                safe_write(&mut tf, line.as_bytes());
                
                break;
            }

            LogMsg::SessionEnd { reason, exit_status } => {
                flush_output!();
                let now = chrono::Utc::now();
                    let mut rec = serde_json::json!({
                        "timestamp": now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
                        "type": "session_end",
                        "reason": reason,
                        "exit_status": exit_status,
                    });
                    if let Some(m) = &meta { rec["session_id"] = m.session_id.clone().into(); }
                    let mut line = rec.to_string(); line.push('\n');
                    safe_write(&mut jf, line.as_bytes());

                let line = format!(
                    "[{}] [SESSION END] reason={reason} exit={}\n",
                    now.format("%Y-%m-%d %H:%M:%S%.3f"),
                    exit_status.map(|x| x.to_string()).unwrap_or("-".into())
                    );

                safe_write(&mut tf, line.as_bytes());
  
                break;
            }

            LogMsg::Resize { rows, cols } => {
                let now = chrono::Utc::now();
                    let mut rec = serde_json::json!({
                        "timestamp": now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
                        "type": "resize", "rows": rows, "cols": cols,
                    });
                    if let Some(m) = &meta { rec["session_id"] = m.session_id.clone().into(); }
                    let mut line = rec.to_string(); line.push('\n');
                    safe_write(&mut jf, line.as_bytes());
                // Text log: resize events are informational only, not logged by default.
            }

            LogMsg::SessionStart(m) => {
                meta = Some(*m.clone());
                session_start_time = Instant::now();
                let now = chrono::Utc::now();
                let username = m.user.as_deref().unwrap_or("?");
                if format.write_text() {
                    let hostname = m.host
                        .as_deref()
                        .unwrap_or("unknown")
                        .split('.')
                        .next()
                        .unwrap_or("unknown")
                        .to_ascii_lowercase();

                   let user = safe_filename_component(
                        m.user.as_deref().unwrap_or("unknown")
                    );

                    let ts = chrono::DateTime::parse_from_rfc3339(&m.started_at)
                        .map(|dt| dt.format("%Y%m%d-%H%M%S-%3f").to_string())
                        .unwrap_or_else(|_| {
                            chrono::Utc::now()
                                .format("%Y%m%d-%H%M%S-%3f")
                                .to_string()
                        });

                    let filename = format!(
                        "{}-{}-{}.log",
                        hostname,
                        user,
                        ts
                    );

                    let path = text_log_dir.join(filename);

                    tf = fs::OpenOptions::new().create(true).append(true).open(&path)
                        .ok();

                    if tf.is_some() {
                        let _ = fs::set_permissions(
                            &path,
                            fs::Permissions::from_mode(0o644)
                        );
                    }
                }

                    let line = format!(
                        "[{}] [SESSION START] user={} uid={} host={} tty={} cwd={} ip={}\n",
                        now.format("%Y-%m-%d %H:%M:%S%.3f"),
                        username,
                        m.uid.unwrap_or(0),
                        m.host.as_deref().unwrap_or("-"),
                        tty_name(STDIN_FILENO),
                        m.cwd.as_deref().unwrap_or("-"),
                        m.remote_ip.as_deref().unwrap_or("-"),
                    );
                    safe_write(&mut tf, line.as_bytes());

                    let mut rec = serde_json::to_value(&*m).unwrap_or(serde_json::Value::Null);
                    rec["timestamp"] = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string().into();
                    rec["type"] = "session_start".into();
                    let mut line = rec.to_string(); line.push('\n');
                    safe_write(&mut jf, line.as_bytes());
            }

            LogMsg::Flush => {
                if !out_lines.is_empty() && last_output.elapsed() > Duration::from_millis(100) {
                    flush_output!();
                }
                if let Some(ref mut f) = tf { let _ = f.flush(); }
                if let Some(ref mut f) = jf { let _ = f.flush(); }
            }

            LogMsg::Entry { source, pid, content } => {
                out_pid = pid;

                // Detect new PIDs (sudo/su) and emit user-change events.
                if pid > 0 && !pid_chain.iter().any(|(p, _)| *p == pid) {
                    let user = pid_username(pid).unwrap_or_else(|| "?".to_string());
                    pid_chain.push((pid, user.clone()));
                    if pid_chain.len() > 1 {
                        let chain_str: String = pid_chain.iter()
                            .map(|(p, u)| format!("{u}({p})"))
                            .collect::<Vec<_>>().join(" -> ");
                        let prev = pid_chain[pid_chain.len()-2].1.clone();
                        let now  = chrono::Utc::now();
                            let line = format!(
                                "[{}] [USER CHANGE] {}\n",
                                now.format("%Y-%m-%d %H:%M:%S%.3f"), chain_str
                            );
                            safe_write(&mut tf, line.as_bytes());

                            let mut rec = serde_json::json!({
                                "timestamp": now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
                                "type": "user_change", "pid": pid,
                                "user": user, "prev_user": prev, "pid_chain": chain_str,
                            });
                            if let Some(m) = &meta { rec["session_id"] = m.session_id.clone().into(); }
                            let mut line = rec.to_string(); line.push('\n');
                            safe_write(&mut jf, line.as_bytes());

                        session_start_time = Instant::now();
                    }
                }

                match source {
                    "STDIN" => {
                        flush_output!();
                        let cmd = String::from_utf8_lossy(&content).trim().to_string();
                        if !cmd.is_empty() {
                            last_stdin = Some(cmd.clone());
                            write_entry(
                                &mut tf,
                                &mut jf,
                                "STDIN",
                                pid,
                                cmd.as_bytes(),
                                meta.as_ref().map(|m| m.session_id.as_str()),
                                meta.as_ref().and_then(|m| m.user.as_deref()),
                            );
                        }
                    }
                    _ => {
                        if session_start_time.elapsed() < shell_init_quiet { continue; }
                        last_output = Instant::now();

                        // Output frames: apply ANSI strip only for text log rendering.
                        let clean = if strip { strip_ansi_bytes(&content) } else { content };

                        out_buf.extend_from_slice(&clean);
                        while let Some(pos) = out_buf[out_start..].iter().position(|&b| b == b'\n') {
                            let abs_pos = out_start + pos;
                            let raw: Vec<u8> = out_buf[out_start..=abs_pos].to_vec();
                            out_start = abs_pos + 1;
                            let end = raw.iter().rposition(|&b| b > b' ').map(|p| p+1).unwrap_or(0);
                            if end == 0 { continue; }
                            let line = String::from_utf8_lossy(&raw[..end]).to_string();
                            if is_prompt_line(&line) { continue; }
                            // Suppress shell echo of the submitted command.
                            if let Some(cmd) = &last_stdin {
                                if line.contains(cmd) &&
                                   (line.contains("$ ") ||
                                    line.contains("# ") ||
                                    line.contains("% ") ||
                                    line.contains("> "))
                                {
                                    last_stdin = None;
                                    continue;
                                }
                            }
                            if out_lines.last().map(|l| l == &line).unwrap_or(false) { continue; }
                            out_lines.push(line);
                        }
                        if out_start > out_buf.len() / 2 {
                            out_buf.drain(..out_start);
                            out_start = 0;
                        }
                    }
                }
            }
        }
    }
    if let Some(ref mut f) = tf { let _ = f.flush(); }
    if let Some(ref mut f) = jf { let _ = f.flush(); }
}

fn write_entry(tf: &mut Option<fs::File>, jf: &mut Option<fs::File>,
               source: &str, pid: u32, content: &[u8],
               session_id: Option<&str>, username: Option<&str>) {
    let now  = chrono::Utc::now();
    let text = String::from_utf8_lossy(content);
    let user = username.unwrap_or("?");

        let line = format!(
            "[{}] [{}] [{}] [PID: {}] {}\n",
            now.format("%Y-%m-%d %H:%M:%S%.3f"), source, user, pid, text
        );
        safe_write(tf, line.as_bytes());

        let mut rec = serde_json::json!({
            "timestamp":  now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            "type":       source.to_lowercase(),
            "source":     source,
            "process_id": pid,
            "content":    text,
        });
        if let Some(sid) = session_id {
            rec["session_id"] = serde_json::Value::String(sid.to_owned());
        }
        if let Some(u) = username {
            rec["username"] = serde_json::Value::String(u.to_owned());
        }
        let mut line = rec.to_string(); line.push('\n');
        safe_write(jf, line.as_bytes());

}

fn safe_write(file: &mut Option<std::fs::File>, data: &[u8]) {
    if let Some(f) = file {
        if let Err(e) = f.write_all(data) {
            match e.raw_os_error() {
                Some(libc::ENOSPC)
                | Some(libc::EDQUOT)
                | Some(libc::EROFS) => {
                    thread_local! {
                        static LAST_WARN: std::cell::Cell<Option<Instant>> = std::cell::Cell::new(None);
                    }
                    LAST_WARN.with(|last| {
                        let now = Instant::now();
                        let should_warn = last.get().map(|t| now - t > Duration::from_secs(30)).unwrap_or(true);
                        if should_warn {
                            eprintln!("[telescreen] write failed: {} (disk space, retrying)", e);
                            last.set(Some(now));
                        }
                    });
                }
                _ => {
                    eprintln!("[telescreen] write failed: {}", e);
                }
            }
        }
    }
}


fn is_prompt_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() { return false; }
    let ends_prompt = t.ends_with('$') || t.ends_with('#') || t.ends_with('>') || t.ends_with('%');
    if !ends_prompt { return false; }
    t.contains('@') || t.contains(":~") || t.contains(":/") || t.contains(": ")
}

// ─────────────────────────────────────────────────────────────────────────────
// Control socket thread
// ─────────────────────────────────────────────────────────────────────────────

fn control_thread(listener: UnixListener, pid_file: String,
                  output_log: String, json_log: String) {
    for stream in listener.incoming() {
        if let Ok(mut s) = stream {
            let uid = {
                use std::os::unix::io::AsRawFd;
                let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
                let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
                let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET,
                    libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len) };
                if r == 0 { cred.uid } else { u32::MAX }
            };
            if uid != 0 { let _ = s.write_all(b"ERR: permission denied\n"); continue; }

            let mut buf = [0u8; 64];
            let n = s.read(&mut buf).unwrap_or(0);
            match String::from_utf8_lossy(&buf[..n]).trim().to_uppercase().as_str() {
                "STOP"  => { SHUTDOWN.store(true, Ordering::Relaxed); let _ = s.write_all(b"OK: stopping\n"); std::thread::sleep(Duration::from_millis(500)); remove_pid_file(&pid_file); std::process::exit(0); }
                "FLUSH" => { let _ = s.write_all(b"OK: flushed\n"); }
                "STATUS" => {
                    let ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_secs()).unwrap_or(0);
                    let resp = serde_json::json!({
                        "unix_time": ts,
                        "output_log": output_log,
                        "json_log": json_log,
                    });
                    let _ = s.write_all(resp.to_string().as_bytes());
                    let _ = s.write_all(b"\n");
                }
                _ => { let _ = s.write_all(b"ERR: unknown command\n"); }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// install / uninstall
// ─────────────────────────────────────────────────────────────────────────────

const PROFILE_D:    &str = "/etc/profile.d/telescreen.sh";
const PROFILE_MARK: &str = "# managed by telescreen";

fn profile_snippet(exe: &str, agent_sock: &str) -> String {
    let mut s = String::new();
    s.push_str(PROFILE_MARK); s.push('\n');
    s.push_str("case $- in *i*) ;; *) return ;; esac\n");
    s.push_str("[ -n \"$TELESCREEN_SESSION\" ] && return\n");
    s.push_str(&format!("[ -S \"{}\" ] || return\n", agent_sock));
    s.push_str("export TELESCREEN_SESSION=1\n");
    s.push_str(&format!("exec \"{}\" session\n", exe));
    s
}

pub fn install(cfg: &TelescreenConfig) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot find own path: {e}"))?;
    let exe_str = exe.to_string_lossy();

    fs::write(PROFILE_D, profile_snippet(&exe_str, &cfg.agent_sock))
        .map_err(|e| format!("cannot write {PROFILE_D}: {e}\nTry: sudo telescreen install"))?;
    let _ = fs::set_permissions(PROFILE_D, fs::Permissions::from_mode(0o644));
    println!("[telescreen] Written {PROFILE_D}");

    let cfg_path = crate::config::TelescreenConfig::write_default(Some("/etc/telescreen/config.yaml"))?;
    println!("[telescreen] Config: {}", cfg_path.display());

    for p in [&cfg.output_log, &cfg.json_log, &cfg.diag_log,
              &cfg.pid_file, &cfg.agent_sock, &cfg.ctrl_sock] {
        if let Some(dir) = std::path::Path::new(p).parent() {
            if fs::create_dir_all(dir).is_ok() {
                let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
            }
        }
    }
    println!("\n[telescreen] Install complete. Next: sudo telescreen start");
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    match fs::remove_file(PROFILE_D) {
        Ok(_) => println!("[telescreen] Removed {PROFILE_D}"),
        Err(e) if e.kind() == io::ErrorKind::NotFound
               => println!("[telescreen] {PROFILE_D} not found"),
        Err(e) => return Err(format!("remove {PROFILE_D}: {e} (try sudo)")),
    }
    Ok(())
}

pub fn send_control(cfg: &TelescreenConfig, command: &str) -> Result<(), String> {
    let mut s = UnixStream::connect(&cfg.ctrl_sock)
        .map_err(|e| format!("Cannot connect to '{}': {e}", cfg.ctrl_sock))?;
    s.write_all(command.as_bytes()).map_err(|e| format!("write: {e}"))?;
    s.shutdown(std::net::Shutdown::Write).ok();
    let mut r = String::new();
    s.read_to_string(&mut r).map_err(|e| format!("read: {e}"))?;
    print!("{r}");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Terminal helpers
// ─────────────────────────────────────────────────────────────────────────────

fn get_winsize(fd: RawFd) -> Option<Winsize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_row > 0 {
        Some(Winsize { ws_row: ws.ws_row, ws_col: ws.ws_col,
                       ws_xpixel: ws.ws_xpixel, ws_ypixel: ws.ws_ypixel })
    } else { None }
}
fn set_raw_mode(fd: RawFd) -> libc::termios {
    let mut orig: libc::termios = unsafe { std::mem::zeroed() };
    unsafe { libc::tcgetattr(fd, &mut orig) };
    let mut raw = orig;
    unsafe { libc::cfmakeraw(&mut raw) };
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
    orig
}
fn restore_termios(fd: RawFd, t: &libc::termios) {
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, t) };
}

fn safe_filename_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}


fn open_append_nofollow(path: &str) -> io::Result<fs::File> {
    let cpath = std::ffi::CString::new(path)?;

    let fd = unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_WRONLY
                | libc::O_CREAT
                | libc::O_APPEND
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW,
            0o644,
        )
    };

    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

// ─────────────────────────────────────────────────────────────────────────────
// PID file helpers
// ─────────────────────────────────────────────────────────────────────────────

fn write_pid_file_raw(path: &str, pid: u32) {
    let s = format!("{pid}\n");
    let c = CString::new(path).unwrap_or_default();
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_WRONLY|libc::O_CREAT|libc::O_TRUNC, 0o644i32) };
    if fd >= 0 { unsafe { libc::write(fd, s.as_ptr() as *const libc::c_void, s.len()); libc::close(fd); } }
}
fn redirect_stdio_to_devnull() {
    let c = CString::new("/dev/null").unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR, 0i32) };
    if fd >= 0 { unsafe { libc::dup2(fd, STDIN_FILENO); libc::dup2(fd, STDOUT_FILENO);
        libc::dup2(fd, STDERR_FILENO); if fd > 2 { libc::close(fd); } } }
}
pub fn read_pid_file(path: &str) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}
pub fn remove_pid_file(path: &str) { let _ = fs::remove_file(path); }
pub fn process_is_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}
