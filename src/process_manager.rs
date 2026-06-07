use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::process::Command;
use tokio::io::{BufReader};

use crate::config::Config;
use crate::logger::{Logger, StreamSource};
use crate::stream_interceptor::{intercept_output, intercept_stdin};

pub struct ProcessManager;

impl ProcessManager {
    /// Spawn the child process, wire all streams, run until exit.
    /// Returns the child's exit code.
    pub async fn run(config: Config) -> Result<i32, String> {
        let config = Arc::new(config);

        // ── Spawn child ────────────────────────────────────────────────────────
        let mut cmd = Command::new(&config.target);
        cmd.args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Keep child in the same process group for signal forwarding.
            .kill_on_drop(false);

        let mut child = cmd.spawn().map_err(|e| {
            format!("Failed to spawn '{}': {e}", config.target)
        })?;

        let pid = child.id().unwrap_or(0);

        // ── Logger ─────────────────────────────────────────────────────────────
        let (logger, writer_handle) = Logger::spawn(Arc::clone(&config));
        let log_tx = logger.sender();

        // ── Take stream handles ────────────────────────────────────────────────
        let child_stdout = child.stdout.take()
            .ok_or("Could not take child stdout")?;
        let child_stderr = child.stderr.take()
            .ok_or("Could not take child stderr")?;
        let child_stdin_pipe = child.stdin.take()
            .ok_or("Could not take child stdin")?;

        // ── Parent terminal streams ────────────────────────────────────────────
        // We use raw tokio equivalents of stdin / stdout / stderr.
        let parent_stdin  = tokio::io::stdin();
        let parent_stdout = tokio::io::stdout();
        let parent_stderr = tokio::io::stderr();

        // ── Signal handling ────────────────────────────────────────────────────
        // Set a flag on Ctrl-C so we can forward the signal to the child and
        // then flush logs before exiting.
        let interrupted = Arc::new(AtomicBool::new(false));
        let interrupted_flag = Arc::clone(&interrupted);

        // ctrlc sends SIGINT on Unix and catches Ctrl-C on Windows.
        if let Err(e) = ctrlc::set_handler(move || {
            interrupted_flag.store(true, Ordering::SeqCst);
        }) {
            eprintln!("[telescreen] Warning: cannot set Ctrl-C handler: {e}");
        }

        // ── Launch stream interceptor tasks ────────────────────────────────────
        let mut tasks = tokio::task::JoinSet::new();

        // stdout interceptor
        if config.filter.stdout {
            let tx = log_tx.clone();
            tasks.spawn(async move {
                let reader = BufReader::new(child_stdout);
                intercept_output(reader, parent_stdout, StreamSource::Stdout, pid, tx).await;
            });
        }

        // stderr interceptor
        if config.filter.stderr {
            let tx = log_tx.clone();
            tasks.spawn(async move {
                let reader = BufReader::new(child_stderr);
                intercept_output(reader, parent_stderr, StreamSource::Stderr, pid, tx).await;
            });
        }

        // stdin interceptor (forward parent stdin → child stdin + log)
        if config.filter.stdin {
            let tx = log_tx.clone();
            tasks.spawn(async move {
                intercept_stdin(parent_stdin, child_stdin_pipe, pid, tx).await;
            });
        }

        // ── Wait for child to exit ─────────────────────────────────────────────
        // We poll the child in a loop so we can also watch for the Ctrl-C flag.
        let exit_status = loop {
            // Check if Ctrl-C was received; if so, kill the child.
            if interrupted.load(Ordering::SeqCst) {
                eprintln!("\n[telescreen] Interrupted — terminating child process {pid}.");
                let _ = child.kill().await;
                break child.wait().await;
            }

            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    // Child still running; yield and try again shortly.
                    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                }
                Err(e) => {
                    eprintln!("[telescreen] Warning: error waiting for child: {e}");
                    break Err(e);
                }
            }
        };

        // ── Drain remaining stream data ────────────────────────────────────────
        // Allow interceptor tasks to finish reading EOF before we shut the logger.
        while (tasks.join_next().await).is_some() {}

        // ── Graceful shutdown ──────────────────────────────────────────────────
        logger.shutdown();
        // Wait for the writer task to flush everything to disk.
        let _ = writer_handle.await;

        // ── Return exit code ───────────────────────────────────────────────────
        let code = match exit_status {
            Ok(status) => status.code().unwrap_or(1),
            Err(_) => 1,
        };

        Ok(code)
    }
}
