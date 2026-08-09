use std::io::{Read, Write};
use std::time::Duration;

use charger_core::error::ChargerError;

use crate::display;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

fn socket_path() -> String {
    charger_core::config::schema::DEFAULT_CONFIG_PATH.replace("config.toml", "daemon.sock")
}

pub fn run(enable: bool) -> Result<(), ChargerError> {
    #[cfg(unix)]
    {
        let command = if enable {
            b"bypass on".as_slice()
        } else {
            b"bypass off".as_slice()
        };

        let path = socket_path();

        let mut stream = match UnixStream::connect(&path) {
            Ok(stream) => stream,
            Err(_) => {
                display::error(
                    "Daemon is not running. Bypass requires the daemon to hold the state.",
                );
                return Ok(());
            }
        };

        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));

        if let Err(e) = stream.write_all(command) {
            display::error(&format!("Failed to send bypass command: {e}"));
            return Ok(());
        }

        let mut response = String::new();

        match stream.read_to_string(&mut response) {
            Ok(_) => {
                let response = response.trim();

                if response.starts_with("OK") {
                    display::success(response);
                } else {
                    display::error(if response.is_empty() {
                        "Daemon returned an empty response"
                    } else {
                        response
                    });
                }
            }

            Err(e) => {
                display::error(&format!("Failed to read daemon response: {e}"));
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = enable;
        display::error("Bypass mode is only supported on UNIX/Android.");
    }

    Ok(())
}
