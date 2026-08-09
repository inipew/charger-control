use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChargerError {
    #[error("Failed to read sysfs node {path}: {source}")]
    SysfsRead { path: PathBuf, #[source] source: std::io::Error },

    #[error("Failed to write sysfs node {path}: {source}")]
    SysfsWrite { path: PathBuf, #[source] source: std::io::Error },

    #[error("No known charging control node found on this device")]
    NoChargingNodeFound,

    #[error("Partial write failure: {succeeded} succeeded, {failed} failed")]
    PartialWriteFailure { succeeded: usize, failed: usize },

    #[error("Failed to parse value '{0}' from sysfs")]
    ParseError(&'static str),

    #[error("Config file read error at {path}: {source}")]
    ConfigRead { path: PathBuf, #[source] source: std::io::Error },

    #[error("Config file write error at {path}: {source}")]
    ConfigWrite { path: PathBuf, #[source] source: std::io::Error },

    #[error("Config parse error: {0}")]
    ConfigParse(String),

    #[error("Config serialize error: {0}")]
    ConfigSerialize(String),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("Daemon not running (no socket at {0})")]
    DaemonNotRunning(String),
}
