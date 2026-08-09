use crate::error::ChargerError;
use std::path::Path;
use std::fs;

pub trait HardwareIo: Send + Sync {
    fn read(&self, path: &Path) -> Result<String, ChargerError>;
    fn write(&self, path: &Path, value: &str) -> Result<(), ChargerError>;
}

pub struct SysfsIo;

impl HardwareIo for SysfsIo {
    fn read(&self, path: &Path) -> Result<String, ChargerError> {
        fs::read_to_string(path).map_err(|e| ChargerError::SysfsRead {
            path: path.to_path_buf(),
            source: e,
        })
    }

    fn write(&self, path: &Path, value: &str) -> Result<(), ChargerError> {
        use std::fs::OpenOptions;
        use std::io::Write;

        OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|mut f| f.write_all(value.as_bytes()))
            .map_err(|e| ChargerError::SysfsWrite {
                path: path.to_path_buf(),
                source: e,
            })
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::path::PathBuf;

    #[derive(Clone)]
    pub struct MockHardwareIo {
        nodes: Arc<Mutex<HashMap<PathBuf, String>>>,
        read_errors: Arc<Mutex<HashMap<PathBuf, std::io::ErrorKind>>>,
        write_errors: Arc<Mutex<HashMap<PathBuf, std::io::ErrorKind>>>,
    }

    impl Default for MockHardwareIo {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockHardwareIo {
        pub fn new() -> Self {
            Self {
                nodes: Arc::new(Mutex::new(HashMap::new())),
                read_errors: Arc::new(Mutex::new(HashMap::new())),
                write_errors: Arc::new(Mutex::new(HashMap::new())),
            }
        }

        pub fn set_node(&self, path: &Path, value: &str) {
            self.nodes.lock().unwrap().insert(path.to_path_buf(), value.to_string());
        }

        pub fn get_node(&self, path: &Path) -> Option<String> {
            self.nodes.lock().unwrap().get(path).cloned()
        }

        pub fn inject_read_error(&self, path: &Path, error: std::io::ErrorKind) {
            self.read_errors.lock().unwrap().insert(path.to_path_buf(), error);
        }

        pub fn inject_write_error(&self, path: &Path, error: std::io::ErrorKind) {
            self.write_errors.lock().unwrap().insert(path.to_path_buf(), error);
        }
    }

    impl HardwareIo for MockHardwareIo {
        fn read(&self, path: &Path) -> Result<String, ChargerError> {
            if let Some(err) = self.read_errors.lock().unwrap().get(path) {
                return Err(ChargerError::SysfsRead {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(*err, "injected error"),
                });
            }

            self.nodes
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| ChargerError::SysfsRead {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
                })
        }

        fn write(&self, path: &Path, value: &str) -> Result<(), ChargerError> {
            if let Some(err) = self.write_errors.lock().unwrap().get(path) {
                return Err(ChargerError::SysfsWrite {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(*err, "injected error"),
                });
            }

            self.nodes.lock().unwrap().insert(path.to_path_buf(), value.to_string());
            Ok(())
        }
    }
}
