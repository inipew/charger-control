use std::path::Path;
use crate::error::ChargerError;
use std::fs;

pub trait PersistenceIo: Send + Sync {
    fn read_state(&self, path: &Path) -> Result<String, ChargerError>;
    fn write_state(&self, path: &Path, content: &str) -> Result<(), ChargerError>;
    fn delete_state(&self, path: &Path) -> Result<(), ChargerError>;
}

pub struct FilePersistenceIo;

impl PersistenceIo for FilePersistenceIo {
    fn read_state(&self, path: &Path) -> Result<String, ChargerError> {
        fs::read_to_string(path).map_err(|e| ChargerError::StateError {
            path: path.to_path_buf(),
            source: e,
        })
    }

    fn write_state(&self, path: &Path, content: &str) -> Result<(), ChargerError> {
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return Err(ChargerError::StateError {
                    path: parent.to_path_buf(),
                    source: e,
                });
            }
        }

        let tmp = path.with_extension("tmp");
        fs::write(&tmp, content).map_err(|e| ChargerError::StateError {
            path: tmp.clone(),
            source: e,
        })?;

        fs::rename(&tmp, path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            ChargerError::StateError {
                path: path.to_path_buf(),
                source: e,
            }
        })
    }

    fn delete_state(&self, path: &Path) -> Result<(), ChargerError> {
        fs::remove_file(path).map_err(|e| ChargerError::StateError {
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

    #[derive(Clone)]
    pub struct MockPersistenceIo {
        states: Arc<Mutex<HashMap<PathBuf, String>>>,
        read_errors: Arc<Mutex<HashMap<PathBuf, std::io::ErrorKind>>>,
        write_errors: Arc<Mutex<HashMap<PathBuf, std::io::ErrorKind>>>,
    }

    impl MockPersistenceIo {
        pub fn new() -> Self {
            Self {
                states: Arc::new(Mutex::new(HashMap::new())),
                read_errors: Arc::new(Mutex::new(HashMap::new())),
                write_errors: Arc::new(Mutex::new(HashMap::new())),
            }
        }

        pub fn inject_read_error(&self, path: &Path, error: std::io::ErrorKind) {
            self.read_errors.lock().unwrap().insert(path.to_path_buf(), error);
        }

        pub fn inject_write_error(&self, path: &Path, error: std::io::ErrorKind) {
            self.write_errors.lock().unwrap().insert(path.to_path_buf(), error);
        }
    }

    impl PersistenceIo for MockPersistenceIo {
        fn read_state(&self, path: &Path) -> Result<String, ChargerError> {
            if let Some(err) = self.read_errors.lock().unwrap().get(path) {
                return Err(ChargerError::StateError {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(*err, "injected error"),
                });
            }

            self.states
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| ChargerError::StateError {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
                })
        }

        fn write_state(&self, path: &Path, content: &str) -> Result<(), ChargerError> {
            if let Some(err) = self.write_errors.lock().unwrap().get(path) {
                return Err(ChargerError::StateError {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(*err, "injected error"),
                });
            }

            self.states.lock().unwrap().insert(path.to_path_buf(), content.to_string());
            Ok(())
        }

        fn delete_state(&self, path: &Path) -> Result<(), ChargerError> {
            self.states.lock().unwrap().remove(path);
            Ok(())
        }
    }
}
