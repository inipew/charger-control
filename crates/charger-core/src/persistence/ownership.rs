use crate::battery::control;
use crate::error::ChargerError;
use std::path::{Path, PathBuf};
use crate::persistence::io::PersistenceIo;
use crate::hardware::io::HardwareIo;

const STATE_FILE: &str = "/data/adb/charger-control/ownership.state";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    NotOwned,
    Owned { original_charging: bool },
}

pub enum RecoveryStatus {
    NotNeeded,
    Recovered,
}

pub fn load_persistent_ownership(io: &dyn PersistenceIo) -> Option<bool> {
    let content = io.read_state(Path::new(STATE_FILE)).ok()?;

    match content.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

pub fn save_persistent_ownership(original: bool, io: &dyn PersistenceIo) -> Result<(), ChargerError> {
    let value = if original { "1" } else { "0" };
    io.write_state(Path::new(STATE_FILE), value)
}

pub fn clear_persistent_ownership(io: &dyn PersistenceIo) {
    match io.delete_state(Path::new(STATE_FILE)) {
        Ok(()) => {}
        Err(ChargerError::StateError { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::error!(
                "Failed to clear ownership state: {}",
                e
            );
        }
    }
}

pub fn recover_stale_ownership(profile: &crate::hardware::profile::HardwareProfile, hw_io: &dyn HardwareIo, pers_io: &dyn PersistenceIo) -> Result<RecoveryStatus, ChargerError> {
    let Some(original) = load_persistent_ownership(pers_io) else {
        return Ok(RecoveryStatus::NotNeeded);
    };

    tracing::warn!(
        "Found stale ownership state (original charging={}). \
         Daemon likely crashed previously. Restoring hardware state...",
        original
    );

    match control::set_charging(original, profile, hw_io) {
        Ok(res) if res.all_succeeded() => {
            tracing::info!("Stale ownership recovered successfully.");
            clear_persistent_ownership(pers_io);
            Ok(RecoveryStatus::Recovered)
        }
        Ok(res) => {
            tracing::error!(
                "Partial failure during stale ownership recovery. ({} succeeded, {} failed)",
                res.succeeded,
                res.failed
            );
            // Keep the state file to try again next time
            Err(ChargerError::StateError { path: PathBuf::from(STATE_FILE), source: std::io::Error::other("Partial recovery failure") })
        }
        Err(e) => {
            tracing::error!(
                "Failed to recover stale ownership: {}",
                e
            );
            // Keep the state file to try again next time
            Err(e)
        }
    }
}
