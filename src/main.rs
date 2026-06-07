mod cli;
mod config;
mod daemon;

use std::process;
use cli::{Cli, Cmd};
use config::TelescreenConfig;

fn main() {
    let cli = Cli::parse_args();

    // Load config once — all subcommands share it.
    let cfg = TelescreenConfig::load(cli.config.as_deref());

    match cli.command {
        // ── Daemon (must run before any Tokio runtime) ────────────────────────
        #[cfg(unix)]
        Cmd::Start => {
            if let Err(e) = daemon::start(&cfg) {
                eprintln!("[telescreen] {e}"); process::exit(1);
            }
        }

        #[cfg(unix)]
        Cmd::Session => {
            // Foreground bridge — becomes the user's shell transparently.
            if let Err(e) = daemon::run_session(&cfg) {
                eprintln!("[telescreen] {e}"); process::exit(1);
            }
        }

        #[cfg(unix)]
        Cmd::Stop => {
            match daemon::read_pid_file(&cfg.pid_file) {
                Some(pid) if daemon::process_is_alive(pid) => {
                    if let Err(e) = daemon::send_control(&cfg, "STOP") {
                        eprintln!("[telescreen] {e}"); process::exit(1);
                    }
                    daemon::remove_pid_file(&cfg.pid_file);
                    println!("[telescreen] Daemon stopped.");
                }
                _ => { eprintln!("[telescreen] Daemon is not running."); process::exit(1); }
            }
        }

        #[cfg(unix)]
        Cmd::Status => {
            match daemon::read_pid_file(&cfg.pid_file) {
                Some(pid) if daemon::process_is_alive(pid) => {
                    println!("[telescreen] Daemon running (PID {pid}).");
                    let _ = daemon::send_control(&cfg, "STATUS");
                }
                _ => { println!("[telescreen] Daemon is not running."); process::exit(1); }
            }
        }

        #[cfg(unix)]
        Cmd::Flush => {
            if let Err(e) = daemon::send_control(&cfg, "FLUSH") {
                eprintln!("[telescreen] {e}"); process::exit(1);
            }
        }

        #[cfg(unix)]
        Cmd::Install => {
            if let Err(e) = daemon::install(&cfg) {
                eprintln!("[telescreen] {e}"); process::exit(1);
            }
        }

        #[cfg(unix)]
        Cmd::Uninstall => {
            if let Err(e) = daemon::uninstall() {
                eprintln!("[telescreen] {e}"); process::exit(1);
            }
        }

        Cmd::DefaultConfig => {
            match TelescreenConfig::write_default(None) {
                Ok(p) => println!("[telescreen] Default config written to {}", p.display()),
                Err(e) => { eprintln!("[telescreen] {e}"); process::exit(1); }
            }
        }

        #[cfg(not(unix))]
        _ => { eprintln!("[telescreen] This command requires Linux/macOS."); process::exit(1); }
    }
}
