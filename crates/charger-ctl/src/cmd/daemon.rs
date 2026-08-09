use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use charger_core::error::ChargerError;

use crate::display;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

fn socket_path() -> String {
    charger_core::config::schema::DEFAULT_CONFIG_PATH
        .replace("config.toml", "daemon.sock")
}

pub fn run(action: &str) -> Result<(), ChargerError> {
    match action {
        "start" => start_daemon(),
        "stop" => stop_daemon(),
        "status" => status_daemon(),
        "restart" => restart_daemon(),
        "reload" => {
            send_cmd(b"reload");
        }
        _ => {
            display::error("Unknown daemon action");
        }
    }

    Ok(())
}

fn restart_daemon() {
    display::info("Restarting daemon...");

    if is_daemon_running() {
        send_cmd(b"shutdown");

        for _ in 0..20 {
            if !is_daemon_running() {
                break;
            }

            std::thread::sleep(Duration::from_millis(100));
        }

        if is_daemon_running() {
            display::error("Daemon did not stop within timeout.");
            return;
        }

        display::success("Daemon stopped.");
    }

    std::thread::sleep(Duration::from_millis(100));

    start_daemon();
}

fn start_daemon() {
    display::info("Starting daemon...");

    if is_daemon_running() {
        display::warn("Daemon is already running.");
        return;
    }

    #[cfg(unix)]
    {
        let exe_path = match std::env::current_exe() {
            Ok(path) => path,
            Err(e) => {
                display::error(&format!("Failed to determine charger-ctl path: {e}"));
                return;
            }
        };

        let daemon_path = exe_path.with_file_name("charger-daemon");

        if !daemon_path.exists() {
            display::error(&format!(
                "charger-daemon not found: {}",
                daemon_path.display()
            ));
            return;
        }

        use std::os::unix::process::CommandExt;

        let mut command = Command::new(&daemon_path);

        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }

                Ok(())
            });
        }

        match command.spawn() {
            Ok(child) => {
                display::success(&format!(
                    "Daemon started (PID {})",
                    child.id()
                ));
            }

            Err(e) => {
                display::error(&format!(
                    "Failed to start daemon: {e}"
                ));
                return;
            }
        }

        // Give daemon a short amount of time to create socket.
        for _ in 0..20 {
            if is_daemon_running() {
                display::success("Daemon is ready.");
                return;
            }

            std::thread::sleep(Duration::from_millis(100));
        }

        display::warn(
            "Daemon process started, but IPC socket is not ready yet.",
        );
    }

    #[cfg(not(unix))]
    {
        display::error(
            "Native daemon management is only supported on UNIX/Android.",
        );
    }
}

fn stop_daemon() {
    display::info("Stopping daemon...");

    if !is_daemon_running() {
        display::warn("Daemon is not running.");
        return;
    }

    send_cmd(b"shutdown");

    for _ in 0..30 {
        if !is_daemon_running() {
            display::success("Daemon stopped gracefully.");
            return;
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    display::error("Daemon did not stop gracefully.");
}

fn status_daemon() {
    display::info("Checking daemon status...");

    if is_daemon_running() {
        send_cmd(b"status");
    } else {
        display::warn("Daemon is INACTIVE (Dead)");
    }
}

fn is_daemon_running() -> bool {
    #[cfg(unix)]
    {
        let path = socket_path();

        match UnixStream::connect(&path) {
            Ok(stream) => {
                let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                true
            }

            Err(_) => false,
        }
    }

    #[cfg(not(unix))]
    {
        false
    }
}

fn send_cmd(cmd: &[u8]) {
    #[cfg(unix)]
    {
        let path = socket_path();

        let mut stream = match UnixStream::connect(&path) {
            Ok(stream) => stream,
            Err(_) => {
                display::error(
                    "Daemon is not running or socket is missing.",
                );
                return;
            }
        };

        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));

        if let Err(e) = stream.write_all(cmd) {
            display::error(&format!(
                "Failed to send daemon command: {e}"
            ));
            return;
        }

        let mut response = String::new();

        match stream.read_to_string(&mut response) {
            Ok(_) => {
                let response = response.trim();

                if response.starts_with("OK:") {
                    let msg = response
                        .strip_prefix("OK:")
                        .unwrap_or(response)
                        .trim();

                    display::success(msg);
                } else if response.starts_with("OK") {
                    display::success(response);
                } else if response.is_empty() {
                    display::error("Daemon returned an empty response.");
                } else {
                    display::error(response);
                }
            }

            Err(e) => {
                display::error(&format!(
                    "Failed to read daemon response: {e}"
                ));
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = cmd;
        display::error("IPC is only supported on UNIX/Android.");
    }
}