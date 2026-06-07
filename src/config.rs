//! Unified configuration for Telescreen.
//!
//! Single YAML config file, searched in order:
//!   1. --config CLI flag
//!   2. $TELESCREEN_CONFIG env var
//!   3. ~/.config/telescreen/config.yaml
//!   4. /etc/telescreen/config.yaml
//!
//! All fields have defaults — the file is entirely optional.

use std::fs;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};

// ── OutputFormat ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat { Text, Json, #[default] Both }

impl OutputFormat {
    pub fn write_text(&self) -> bool { matches!(self, Self::Text  | Self::Both) }
    pub fn write_json(&self) -> bool { matches!(self, Self::Json  | Self::Both) }
}

// ── TelescreenConfig — the one and only config schema ────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TelescreenConfig {
    // ── Daemon sockets & files ───────────────────────────────────────────────
    /// PID file for the running daemon.
    pub pid_file:    String,
    /// Unix socket where session bridges connect.
    pub agent_sock:  String,
    /// Unix socket for stop/status/flush control commands.
    pub ctrl_sock:   String,
    /// Internal diagnostic log (written with raw libc, survives crashes).
    pub diag_log:    String,

    // ── Log output ───────────────────────────────────────────────────────────
    /// Path to the human-readable text log file.
    pub output_log:  String,
    /// Path to the JSONL structured log file.
    pub json_log:    String,
    /// Which formats to write: text | json | both.
    pub format:      OutputFormat,

    // ── Session ──────────────────────────────────────────────────────────────
    /// The real shell to execute inside the PTY (defaults to $SHELL).
    pub shell:       String,

    // ── Feature flags ────────────────────────────────────────────────────────
    /// Log stdin (keystrokes).
    pub log_stdin:   bool,
    /// Log stdout.
    pub log_stdout:  bool,
    /// Log stderr (wrap mode only; daemon mode uses PTY which merges stdout+stderr).
    pub log_stderr:  bool,
    /// Strip ANSI escape sequences from the text log.
    pub strip_ansi:  bool,
}

impl Default for TelescreenConfig {
    fn default() -> Self {
        Self {
            pid_file:   "/run/telescreen/telescreen.pid".into(),
            agent_sock: "/run/telescreen/agent.sock".into(),
            ctrl_sock:  "/run/telescreen/ctrl.sock".into(),
            diag_log:   "/var/log/telescreen/daemon.log".into(),
            output_log: "/var/log/telescreen/".into(),
            json_log:   "/var/log/telescreen/session.jsonl".into(),
            format:     OutputFormat::Both,
            shell:      std::env::var_os("SHELL").and_then(|s| s.into_string().ok()).unwrap_or_else(|| "/bin/bash".into()),
            log_stdin:  true,
            log_stdout: true,
            log_stderr: true,
            strip_ansi: true,
        }
    }
}

impl TelescreenConfig {
    /// Load config from the first file found; fall back to defaults.
    pub fn load(explicit_path: Option<&str>) -> Self {
        for path in Self::search_paths(explicit_path) {
            if let Ok(txt) = fs::read_to_string(&path) {
                match serde_yaml::from_str::<Self>(&txt) {
                    Ok(cfg) => return cfg,
                    Err(e)  => eprintln!("[telescreen] warning: {}: {e}", path.display()),
                }
            }
        }
        Self::default()
    }

    /// Write a default config file to the user config path and return the path.
    pub fn write_default(dest: Option<&str>) -> Result<PathBuf, String> {
        let path = dest.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/etc/telescreen/config.yaml"));
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))?;
        }
        let header = "\
# Telescreen configuration\n\
# All fields are optional — delete any line to use the built-in default.\n\
# Reload: telescreen stop && telescreen start\n\n";
        let yaml = serde_yaml::to_string(&Self::default())
            .map_err(|e| format!("serialize: {e}"))?;
        fs::write(&path, format!("{header}{yaml}"))
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok(path)
    }

    fn search_paths(explicit: Option<&str>) -> Vec<PathBuf> {
        let mut v = vec![];
        if let Some(p) = explicit                              { v.push(PathBuf::from(p)); }
        if let Ok(p) = std::env::var("TELESCREEN_CONFIG")     { v.push(PathBuf::from(p)); }
        v.push(PathBuf::from("/etc/telescreen/config.yaml"));
        v.push(user_config_path());
        v
    }
}

fn user_config_path() -> PathBuf {
    dirs_next::config_dir()
        .unwrap_or_else(|| PathBuf::from(
            std::env::var("HOME").unwrap_or_else(|_| "/root".into())
        ))
        .join("telescreen")
        .join("config.yaml")
}
