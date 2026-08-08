use crate::display;
use charger_core::error::ChargerError;
use std::process::{Command, Stdio};

pub fn run(action: &str) -> Result<(), ChargerError> {
    match action {
        "start" => start_daemon(),
        "stop" => stop_daemon(),
        "status" => status_daemon(),
        "restart" => {
            stop_daemon();
            std::thread::sleep(std::time::Duration::from_millis(500));
            start_daemon();
        }
        "reload" => {
            send_cmd(b"reload");
        }
        _ => display::error("Unknown action"),
    }
    Ok(())
}

fn start_daemon() {
    display::info("Starting daemon...");

    if is_daemon_running() {
        display::warn("Daemon is already running.");
        return;
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // Cari lokasi charger-daemon di folder yang sama dengan charger-ctl
        let exe_path = std::env::current_exe()
            .unwrap_or_else(|_| std::path::PathBuf::from("/system/bin/charger-ctl"));
        let daemon_path = exe_path.with_file_name("charger-daemon");

        let mut cmd = Command::new(&daemon_path);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        unsafe {
            cmd.pre_exec(|| {
                // 1. Lepaskan dari terminal saat ini (SIGHUP protection)
                libc::setsid();

                // 2. Double-Fork Magic (Mencegah re-attachment terminal)
                match libc::fork() {
                    -1 => Err(std::io::Error::last_os_error()),
                    0 => Ok(()), // Cucu (Grandchild) melanjutkan proses execve ke charger-daemon
                    _pid => libc::_exit(0), // Anak pertama langsung bunuh diri
                }
            });
        }

        match cmd.spawn() {
            Ok(_) => display::success("Daemon started in background (detached)"),
            Err(e) => display::error(&format!("Failed to spawn daemon: {e}")),
        }
    }

    #[cfg(not(unix))]
    display::error("Native daemonization is only supported on UNIX/Android.");
}

fn stop_daemon() {
    display::info("Stopping daemon...");
    if !is_daemon_running() {
        display::warn("Daemon is not running.");
        return;
    }
    send_cmd(b"shutdown");
    // Tunggu sampai socket benar-benar hilang (daemon mati)
    for _ in 0..10 {
        if !is_daemon_running() {
            display::success("Daemon stopped gracefully");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
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
        use std::os::unix::net::UnixStream;
        let sock_path =
            charger_core::config::schema::DEFAULT_CONFIG_PATH.replace("config.toml", "daemon.sock");
        UnixStream::connect(sock_path).is_ok()
    }
    #[cfg(not(unix))]
    false
}

fn send_cmd(cmd: &[u8]) {
    #[cfg(unix)]
    {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;

        let sock_path =
            charger_core::config::schema::DEFAULT_CONFIG_PATH.replace("config.toml", "daemon.sock");
        if let Ok(mut stream) = UnixStream::connect(sock_path) {
            let _ = stream.write_all(cmd);
            let mut buf = String::new();
            let _ = stream.read_to_string(&mut buf);
            if buf.starts_with("OK") {
                let msg = buf
                    .trim_start_matches("OK:")
                    .trim_start_matches("OK")
                    .trim();
                display::success(msg);
            } else {
                display::error(&buf);
            }
        } else {
            display::error("Daemon is not running or socket is missing");
        }
    }

    #[cfg(not(unix))]
    display::error("IPC is only supported on UNIX/Android.");
}
