use crate::error::ChargerError;
use std::path::Path;
use std::fs;

pub trait HardwareIo: Send + Sync {
    fn read(&self, path: &Path) -> Result<String, ChargerError>;
    fn write(&self, path: &Path, value: &str) -> Result<(), ChargerError>;
    fn exists(&self, path: &Path) -> bool;
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
        fs::write(path, value).map_err(|e| ChargerError::SysfsWrite {
            path: path.to_path_buf(),
            source: e,
        })
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
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

        fn exists(&self, path: &Path) -> bool {
            self.nodes.lock().unwrap().contains_key(path)
        }
    }
}
