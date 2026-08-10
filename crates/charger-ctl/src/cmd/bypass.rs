use std::time::Duration;

use charger_core::error::ChargerError;

use crate::{client::IpcClient, display};

pub fn run(enable: bool) -> Result<(), ChargerError> {
    let command = if enable {
        b"bypass on".as_slice()
    } else {
        b"bypass off".as_slice()
    };

    match IpcClient::send_command(command, Duration::from_secs(3)) {
        Ok(response) => {
            if response.starts_with("OK") {
                display::success(&response);
            } else if response.is_empty() {
                display::error("Daemon returned an empty response");
            } else {
                display::error(&response);
            }
        }
        Err(err) => {
            display::error(&err);
        }
    }

    Ok(())
}
