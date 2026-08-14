use std::time::Duration;

#[cfg(unix)]
use charger_core::config::schema::DEFAULT_SOCKET_PATH;

#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

pub struct IpcClient;

impl IpcClient {
    /// Send a command to the daemon IPC socket and return the raw string response.
    pub fn send_command(command: &[u8], timeout: Duration) -> Result<String, String> {
        #[cfg(unix)]
        {
            let mut stream = UnixStream::connect(DEFAULT_SOCKET_PATH)
                .map_err(|e| format!("Daemon is not running or IPC socket unavailable: {e}"))?;

            stream
                .set_write_timeout(Some(timeout))
                .map_err(|e| format!("Failed setting write timeout: {e}"))?;

            stream
                .set_read_timeout(Some(timeout))
                .map_err(|e| format!("Failed setting read timeout: {e}"))?;

            stream
                .write_all(command)
                .map_err(|e| format!("Failed to send command to daemon: {e}"))?;

            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .map_err(|e| format!("Failed to read daemon response: {e}"))?;

            Ok(response.trim().to_string())
        }

        #[cfg(not(unix))]
        {
            let _ = (command, timeout);
            Err("IPC is only supported on UNIX/Android systems".to_string())
        }
    }

    /// Quick check to determine if the daemon IPC socket is active and responsive.
    pub fn is_ready(timeout: Duration) -> bool {
        #[cfg(unix)]
        {
            let mut stream = match UnixStream::connect(DEFAULT_SOCKET_PATH) {
                Ok(stream) => stream,
                Err(_) => return false,
            };

            if stream.set_read_timeout(Some(timeout)).is_err()
                || stream.set_write_timeout(Some(timeout)).is_err()
            {
                return false;
            }

            if stream.write_all(b"status").is_err() {
                return false;
            }

            let mut response = [0u8; 64];
            match stream.read(&mut response) {
                Ok(size) if size > 0 => {
                    let res_str = String::from_utf8_lossy(&response[..size]);
                    res_str.starts_with("OK:")
                }
                _ => false,
            }
        }

        #[cfg(not(unix))]
        {
            let _ = timeout;
            false
        }
    }
}
