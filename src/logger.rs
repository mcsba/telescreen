use std::io::Write;
use std::sync::Arc;
use chrono::Utc;
use serde::Serialize;
use tokio::sync::mpsc::{self, UnboundedSender, UnboundedReceiver};
use tokio::sync::Mutex;

use crate::config::Config;

/// Which I/O stream produced a log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StreamSource {
    Stdout,
    Stderr,
    Stdin,
}

impl StreamSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Stdout => "STDOUT",
            Self::Stderr => "STDERR",
            Self::Stdin => "STDIN",
        }
    }

    pub fn stream_type(&self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Stdin => "stdin",
        }
    }
}

/// A single captured log entry passed through the channel.
#[derive(Debug)]
pub struct LogEntry {
    pub source: StreamSource,
    pub pid: u32,
    pub content: String,
}

/// JSON log record (one per line in the .jsonl file).
#[derive(Serialize)]
struct JsonRecord<'a> {
    timestamp: String,
    source: &'a str,
    process_id: u32,
    stream_type: &'a str,
    content: &'a str,
}

/// The async logger. Send log entries via the returned `LogSender`.
pub struct Logger {
    sender: UnboundedSender<Option<LogEntry>>, // None = sentinel to shut down
}

pub type LogSender = UnboundedSender<Option<LogEntry>>;

impl Logger {
    /// Spawn the async writer task and return the Logger + its sender channel.
    pub fn spawn(config: Arc<Config>) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::unbounded_channel::<Option<LogEntry>>();

        let handle = tokio::spawn(writer_task(rx, config));

        (Self { sender: tx.clone() }, handle)
    }

    /// Get a clone of the internal sender so multiple interceptors can share it.
    pub fn sender(&self) -> LogSender {
        self.sender.clone()
    }

    /// Signal the writer task to flush and exit.
    pub fn shutdown(&self) {
        let _ = self.sender.send(None);
    }
}

/// The async task that drains the channel and writes to disk.
async fn writer_task(
    mut rx: UnboundedReceiver<Option<LogEntry>>,
    config: Arc<Config>,
) {
    // Open log files (append mode, create if missing).
    let text_file: Option<Arc<Mutex<std::fs::File>>> = if config.format.write_text() {
        match open_log_file(&config.output_log) {
            Ok(f) => Some(Arc::new(Mutex::new(f))),
            Err(e) => {
                eprintln!("[telescreen] Warning: Cannot open text log '{}': {e}", config.output_log);
                None
            }
        }
    } else {
        None
    };

    let json_file: Option<Arc<Mutex<std::fs::File>>> = if config.format.write_json() {
        match open_log_file(&config.json_log) {
            Ok(f) => Some(Arc::new(Mutex::new(f))),
            Err(e) => {
                eprintln!("[telescreen] Warning: Cannot open JSON log '{}': {e}", config.json_log);
                None
            }
        }
    } else {
        None
    };

    while let Some(msg) = rx.recv().await {
        let entry = match msg {
            Some(e) => e,
            None => break, // Sentinel: flush and exit
        };

        let now = Utc::now();

        // ── Text log ───────────────────────────────────────────────────────────
        if let Some(ref f) = text_file {
            let ts = now.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
            let escaped = escape_content(&entry.content);
            let line = format!(
                "[{}] [SOURCE: {}] [PID: {}] {}\n",
                ts,
                entry.source.label(),
                entry.pid,
                escaped,
            );
            write_to_file(f, &line).await;
        }

        // ── JSON log ───────────────────────────────────────────────────────────
        if let Some(ref f) = json_file {
            let ts = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
            let record = JsonRecord {
                timestamp: ts,
                source: entry.source.label(),
                process_id: entry.pid,
                stream_type: entry.source.stream_type(),
                content: &entry.content,
            };
            match serde_json::to_string(&record) {
                Ok(mut json) => {
                    json.push('\n');
                    write_to_file(f, &json).await;
                }
                Err(e) => {
                    eprintln!("[telescreen] Warning: JSON serialization failed: {e}");
                }
            }
        }
    }

    // Flush both files on exit (std::fs::File flushes on drop, but be explicit).
    if let Some(f) = text_file {
        let _ = f.lock().await.flush();
    }
    if let Some(f) = json_file {
        let _ = f.lock().await.flush();
    }
}

/// Open a file in append + create mode.
fn open_log_file(path: &str) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// Write bytes to a locked file; emit a warning instead of panicking on error.
async fn write_to_file(file: &Arc<Mutex<std::fs::File>>, data: &str) {
    let mut guard = file.lock().await;
    if let Err(e) = guard.write_all(data.as_bytes()) {
        eprintln!("[telescreen] Warning: log write failed: {e}");
    }
}

/// Escape special characters so log lines stay single-line and parsable.
///
/// Rules:
///   `\n` → `\\n`   (newlines)
///   `\r` → `\\r`   (carriage returns)
///   `\t` → `\\t`   (tabs)
///   `\\` → `\\\\`  (backslashes — must be first!)
fn escape_content(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}
