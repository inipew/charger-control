use crate::battery::control;
use crate::error::ChargerError;
use std::path::{Path, PathBuf};
use crate::persistence::io::PersistenceIo;
use crate::hardware::io::HardwareIo;
use serde::{Serialize, Deserialize};

const STATE_FILE: &str = "/data/adb/charger-control/ownership.state";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipPhase {
    Acquiring,
    Owned,
    Releasing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnershipRecord {
    pub version: u32,
    pub generation: u64,
    pub original_charging: bool,
    pub target_charging: bool,
    pub phase: OwnershipPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    NotOwned,
    Owned { original_charging: bool },
}

pub enum RecoveryResult {
    NotNeeded,
    Recovered,
    Failed { succeeded: usize, failed: usize },
}

pub fn load_persistent_ownership(io: &dyn PersistenceIo) -> Option<OwnershipRecord> {
    let content = io.read(Path::new(STATE_FILE)).ok()?;
    toml::from_str(&content).ok()
}

pub fn save_persistent_ownership(record: &OwnershipRecord, io: &dyn PersistenceIo) -> Result<(), ChargerError> {
    let content = toml::to_string(record)
        .map_err(|e| ChargerError::StateError {
            path: PathBuf::from(STATE_FILE),
            source: std::io::Error::other(format!("Failed to serialize ownership: {}", e)),
        })?;
    
    io.atomic_write(Path::new(STATE_FILE), content.as_bytes())
}

pub fn clear_persistent_ownership(io: &dyn PersistenceIo) {
    match io.remove(Path::new(STATE_FILE)) {
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

pub fn recover_stale_ownership(profile: &crate::hardware::profile::HardwareProfile, hw_io: &dyn HardwareIo, pers_io: &dyn PersistenceIo) -> RecoveryResult {
    let Some(record) = load_persistent_ownership(pers_io) else {
        return RecoveryResult::NotNeeded;
    };

    tracing::warn!(
        "Found stale ownership state: phase={:?}, original_charging={}, target_charging={}. \
         Daemon likely crashed or failed to restore. Attempting recovery...",
        record.phase, record.original_charging, record.target_charging
    );

    // Semua phase → restore ke original_charging:
    //   Acquiring: mungkin belum berhasil menulis, restore ke original
    //   Owned:     daemon crash saat ownership aktif, restore ke original
    //   Releasing: restore sudah dimulai tapi belum selesai, ulangi
    let target = record.original_charging;

    match control::set_charging(target, profile, hw_io) {
        Ok(res) if res.all_succeeded() => {
            tracing::info!(
                "Stale ownership recovered (phase={:?}, restored charging={}, {}/{} nodes succeeded).",
                record.phase, target, res.succeeded, res.attempted
            );
            clear_persistent_ownership(pers_io);
            RecoveryResult::Recovered
        }
        Ok(res) => {
            // Partial atau total failure — JANGAN hapus persistent record.
            // Boot berikutnya akan mencoba lagi.
            tracing::error!(
                "Recovery failed (phase={:?}): {}/{} succeeded, {} failed. \
                 Keeping persistent record for next retry.",
                record.phase, res.succeeded, res.attempted, res.failed
            );
            RecoveryResult::Failed { succeeded: res.succeeded, failed: res.failed }
        }
        Err(e) => {
            tracing::error!(
                "Recovery error (phase={:?}): {}. Will retry.",
                record.phase, e
            );
            RecoveryResult::Failed { succeeded: 0, failed: 0 }
        }
    }
}
