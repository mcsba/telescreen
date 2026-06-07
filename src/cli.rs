use clap::{Parser, Subcommand};

/// Telescreen — transparent terminal session logger.
///
/// Config search order:
///   1. --config
///   2. TELESCREEN_CONFIG
///   3. /etc/telescreen/config.yaml
///   4. ~/.config/telescreen/config.yaml
#[derive(Parser, Debug)]
#[command(name = "telescreen", version, about)]
pub struct Cli {
    /// Path to config file (overrides default search path).
    #[arg(long, global = true)]
    pub config: Option<String>,

    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Start the logging daemon in the background.
    Start,

    /// Stop the running daemon.
    Stop,

    /// Show daemon status and log file paths.
    Status,

    /// Flush log buffers to disk immediately.
    Flush,

    /// Install: write /etc/profile.d/telescreen.sh and create log directories.
    /// Run once with sudo. Does not modify /etc/passwd or any user's shell.
    Install,

    /// Uninstall: remove /etc/profile.d/telescreen.sh.
    Uninstall,

    /// Print the default config to stdout.
    DefaultConfig,

    /// [internal] Start a logged PTY session — called by /etc/profile.d/telescreen.sh.
    #[command(hide = true)]
    Session,
}

impl Cli {
    pub fn parse_args() -> Self { Self::parse() }
}
