use tokio::io::{AsyncRead, AsyncReadExt};
use crate::logger::{LogEntry, LogSender, StreamSource};

/// Buffer size for reading from child streams.
const BUF_SIZE: usize = 8 * 1024; // 8 KiB

/// Reads from `reader` in a loop and sends each chunk to the logger.
/// Also writes a copy to `passthrough` (the parent's stdout/stderr) so the
/// user still sees output in their terminal while it is being logged.
pub async fn intercept_output<R, W>(
    mut reader: R,
    mut passthrough: W,
    source: StreamSource,
    pid: u32,
    log_tx: LogSender,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::AsyncWriteExt;

    let mut buf = vec![0u8; BUF_SIZE];

    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break, // EOF
            Ok(n) => {
                let slice = &buf[..n];

                // Pass through to the terminal so the user sees output live.
                if let Err(e) = passthrough.write_all(slice).await {
                    eprintln!("[telescreen] Warning: passthrough write failed: {e}");
                }

                // Convert to UTF-8 lossily (handles binary output gracefully).
                let content = String::from_utf8_lossy(slice).into_owned();

                let entry = LogEntry { source, pid, content };

                // Non-blocking send; if the channel is closed we just stop.
                if log_tx.send(Some(entry)).is_err() {
                    break;
                }
            }
            Err(e) => {
                // Don't crash the interceptor on transient read errors.
                eprintln!("[telescreen] Warning: read error on {}: {e}", source.label());
                break;
            }
        }
    }
}

/// Reads from the parent's stdin and forwards to both the child's stdin pipe
/// and the logger.
pub async fn intercept_stdin<R, W>(
    mut reader: R,
    mut child_stdin: W,
    pid: u32,
    log_tx: LogSender,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::AsyncWriteExt;

    let mut buf = vec![0u8; BUF_SIZE];

    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break, // EOF (Ctrl-D / pipe closed)
            Ok(n) => {
                let slice = &buf[..n];

                // Forward bytes to the child process.
                if let Err(e) = child_stdin.write_all(slice).await {
                    eprintln!("[telescreen] Warning: stdin forward failed: {e}");
                    break;
                }

                // Log the input.
                let content = String::from_utf8_lossy(slice).into_owned();
                let entry = LogEntry {
                    source: StreamSource::Stdin,
                    pid,
                    content,
                };
                if log_tx.send(Some(entry)).is_err() {
                    break;
                }
            }
            Err(e) => {
                eprintln!("[telescreen] Warning: stdin read error: {e}");
                break;
            }
        }
    }
}
