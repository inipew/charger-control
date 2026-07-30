use charger_core::error::ChargerError;
use crate::display;

pub fn run(enable: bool) -> Result<(), ChargerError> {
    let cmd: &[u8] = if enable { b"bypass on" } else { b"bypass off" };
    
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        use std::io::{Read, Write};
        
        let sock_path = charger_core::config::schema::DEFAULT_CONFIG_PATH.replace("config.toml", "daemon.sock");
        if let Ok(mut stream) = UnixStream::connect(sock_path) {
            let _ = stream.write_all(cmd);
            let mut buf = String::new();
            let _ = stream.read_to_string(&mut buf);
            if buf.starts_with("OK") {
                display::success(&buf);
            } else {
                display::error(&buf);
            }
        } else {
            display::error("Daemon is not running. Bypass requires daemon to hold the state.");
        }
    }
    
    #[cfg(not(unix))]
    display::error("Bypass mode is only supported on UNIX/Android.");

    Ok(())
}
