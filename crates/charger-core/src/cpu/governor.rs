use std::{fs, path::Path};
use crate::error::ChargerError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovMode {
    PowerSave,
    SchedUtil,
}

impl GovMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PowerSave => "powersave",
            Self::SchedUtil => "schedutil",
        }
    }
}

pub fn set_cpu_governor(mode: GovMode) -> Result<(), ChargerError> {
    let path = Path::new("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor");
    if path.exists() {
        fs::write(path, mode.as_str())
            .map_err(|e| ChargerError::SysfsWrite { path: path.to_owned(), source: e })
    } else {
        Ok(()) // Not all devices have this, gracefully ignore
    }
}
