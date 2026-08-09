use std::path::Path;
use crate::error::ChargerError;
use std::fs;

pub trait PersistenceIo: Send + Sync {
    fn read(&self, path: &Path) -> Result<String, ChargerError>;
    fn atomic_write(&self, path: &Path, contents: &[u8]) -> Result<(), ChargerError>;
    fn remove(&self, path: &Path) -> Result<(), ChargerError>;
    fn exists(&self, path: &Path) -> bool;
}

pub struct FilePersistenceIo;

impl PersistenceIo for FilePersistenceIo {
    fn read(&self, path: &Path) -> Result<String, ChargerError> {
        fs::read_to_string(path).map_err(|e| ChargerError::StateError {
            path: path.to_path_buf(),
            source: e,
        })
    }

    fn atomic_write(&self, path: &Path, contents: &[u8]) -> Result<(), ChargerError> {
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return Err(ChargerError::StateError {
                    path: parent.to_path_buf(),
                    source: e,
                });
            }
        }

        let tmp = path.with_extension("tmp");
        fs::write(&tmp, contents).map_err(|e| ChargerError::StateError {
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

    fn remove(&self, path: &Path) -> Result<(), ChargerError> {
        fs::remove_file(path).map_err(|e| ChargerError::StateError {
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
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    pub struct MockPersistenceIo {
        states: Arc<Mutex<HashMap<PathBuf, String>>>,
        read_errors: Arc<Mutex<HashMap<PathBuf, std::io::ErrorKind>>>,
        write_errors: Arc<Mutex<HashMap<PathBuf, std::io::ErrorKind>>>,
    }

    impl Default for MockPersistenceIo {
        fn default() -> Self {
            Self::new()
        }
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
        fn read(&self, path: &Path) -> Result<String, ChargerError> {
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

        fn atomic_write(&self, path: &Path, contents: &[u8]) -> Result<(), ChargerError> {
            if let Some(err) = self.write_errors.lock().unwrap().get(path) {
                return Err(ChargerError::StateError {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(*err, "injected error"),
                });
            }

            self.states.lock().unwrap().insert(path.to_path_buf(), String::from_utf8_lossy(contents).into_owned());
            Ok(())
        }

        fn remove(&self, path: &Path) -> Result<(), ChargerError> {
            self.states.lock().unwrap().remove(path);
            Ok(())
        }

        fn exists(&self, path: &Path) -> bool {
            self.states.lock().unwrap().contains_key(path)
        }
    }
}
