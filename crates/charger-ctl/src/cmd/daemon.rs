use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use charger_core::error::ChargerError;

use crate::display;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

const START_TIMEOUT: Duration = Duration::from_secs(5);

const STOP_TIMEOUT: Duration = Duration::from_secs(5);

const POLL_INTERVAL: Duration = Duration::from_millis(50);

fn socket_path() -> String {
    charger_core::config::schema::DEFAULT_SOCKET_PATH.to_string()
}

fn get_daemon_pid() -> Option<u32> {
    let pid_path = charger_core::config::schema::DEFAULT_PID_PATH;
    let content = std::fs::read_to_string(pid_path).ok()?;
    content.trim().parse::<u32>().ok()
}

#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    let alive = unsafe {
        let res = libc::kill(pid as libc::pid_t, 0);

        res == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    };

    if !alive {
        return false;
    }

    // Verify process name to avoid false positives if PID was reassigned
    if let Ok(comm) = std::fs::read_to_string(format!("/proc/{}/comm", pid)) {
        return comm.trim() == "charger-daemon";
    }

    if let Ok(cmdline) = std::fs::read_to_string(format!("/proc/{}/cmdline", pid)) {
        return cmdline.contains("charger-daemon");
    }

    true
}

#[cfg(not(unix))]
fn is_process_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn is_lock_held() -> bool {
    let lock_path = charger_core::config::schema::DEFAULT_LOCK_PATH;

    let file = match std::fs::OpenOptions::new().write(true).open(lock_path) {
        Ok(file) => file,
        Err(_) => return false,
    };

    use std::os::unix::io::AsRawFd;

    let fd = file.as_raw_fd();

    unsafe {
        if libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) != 0 {
            return true;
        }

        let _ = libc::flock(fd, libc::LOCK_UN);
    }

    false
}

#[cfg(not(unix))]
fn is_lock_held() -> bool {
    false
}

fn is_daemon_running_system() -> (bool, Option<u32>) {
    let pid = get_daemon_pid();

    let ipc_ready = daemon_ipc_ready();

    if ipc_ready {
        return (true, pid);
    }

    if let Some(p) = pid {
        if is_process_alive(p) {
            return (true, Some(p));
        }
    }

    if is_lock_held() {
        return (true, pid);
    }

    (false, pid)
}

fn cleanup_stale_files() {
    let _ = std::fs::remove_file(charger_core::config::schema::DEFAULT_PID_PATH);
    let _ = std::fs::remove_file(socket_path());
}

pub fn run(action: &str) -> Result<(), ChargerError> {
    match action {
        "start" => start_daemon(),
        "stop" => stop_daemon(),
        "status" => status_daemon(),
        "restart" => restart_daemon(),
        "reload" => send_cmd(b"reload"),

        _ => {
            let msg = "Unknown daemon action";
            display::error(msg);
            return Err(ChargerError::InvalidInput(msg.to_string()));
        }
    }

    Ok(())
}

fn start_daemon() {
    display::info("Starting daemon...");

    if daemon_ipc_ready() {
        display::warn("Daemon is already running.");

        return;
    }

    if let Some(p) = get_daemon_pid() {
        if is_process_alive(p) {
            display::warn(&format!(
                "Found zombie daemon process (PID {p}). Terminating..."
            ));

            #[cfg(unix)]
            unsafe {
                let _ = libc::kill(p as libc::pid_t, libc::SIGKILL);
            }

            std::thread::sleep(Duration::from_millis(100));

            cleanup_stale_files();
        }
    }

    fn find_daemon_binary() -> Option<std::path::PathBuf> {
        if let Ok(exe_path) = std::env::current_exe() {
            let same_dir = exe_path.with_file_name("charger-daemon");
            if same_dir.exists() {
                return Some(same_dir);
            }
        }

        let magisk_paths = [
            "/data/adb/modules/charger-control/system/bin/charger-daemon",
            "/data/adb/modules_update/charger-control/system/bin/charger-daemon",
        ];

        for path_str in magisk_paths {
            let path = std::path::PathBuf::from(path_str);
            if path.exists() {
                return Some(path);
            }
        }

        if let Ok(path_env) = std::env::var("PATH") {
            for entry in std::env::split_paths(&path_env) {
                let candidate = entry.join("charger-daemon");
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }

        None
    }

    #[cfg(unix)]
    {
        let daemon_path = match find_daemon_binary() {
            Some(path) => path,
            None => {
                display::error(
                    "charger-daemon binary not found in same directory, Magisk paths, or PATH.",
                );
                return;
            }
        };

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

        let mut child = match command.spawn() {
            Ok(child) => child,

            Err(error) => {
                display::error(&format!("Failed to start daemon: {error}"));

                return;
            }
        };

        let pid = child.id();

        display::success(&format!("Daemon started (PID {pid})"));

        let deadline = Instant::now() + START_TIMEOUT;

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    display::error(&format!("Daemon exited during startup: {status}"));

                    return;
                }

                Ok(None) => {}

                Err(error) => {
                    display::error(&format!("Failed checking daemon process: {error}"));

                    return;
                }
            }

            if daemon_ipc_ready() {
                display::success("Daemon is ready.");

                return;
            }

            if Instant::now() >= deadline {
                display::error(
                    "Daemon process is running, \
                     but IPC did not become ready.",
                );

                return;
            }

            std::thread::sleep(POLL_INTERVAL);
        }
    }

    #[cfg(not(unix))]
    {
        display::error("Native daemon management is only supported on UNIX/Android.");
    }
}

fn stop_daemon() {
    display::info("Stopping daemon...");

    let (running, pid) = is_daemon_running_system();

    if !running {
        display::warn("Daemon is not running.");

        cleanup_stale_files();

        return;
    }

    let target_pid = pid.or_else(get_daemon_pid);

    if daemon_ipc_ready() {
        send_cmd(b"shutdown");
    } else if let Some(p) = target_pid {
        #[cfg(unix)]
        unsafe {
            let _ = libc::kill(p as libc::pid_t, libc::SIGTERM);
        }
    }

    let deadline = Instant::now() + STOP_TIMEOUT;

    while Instant::now() < deadline {
        let (still_running, _) = is_daemon_running_system();

        if !still_running {
            cleanup_stale_files();

            display::success("Daemon stopped gracefully.");

            return;
        }

        std::thread::sleep(POLL_INTERVAL);
    }

    if let Some(p) = target_pid {
        #[cfg(unix)]
        {
            display::warn(&format!(
                "Daemon (PID {p}) did not exit within timeout. Sending SIGTERM..."
            ));

            unsafe {
                let _ = libc::kill(p as libc::pid_t, libc::SIGTERM);
            }
        }
    }

    let deadline = Instant::now() + Duration::from_secs(3);

    while Instant::now() < deadline {
        let (still_running, _) = is_daemon_running_system();

        if !still_running {
            cleanup_stale_files();

            display::success("Daemon stopped via SIGTERM.");

            return;
        }

        std::thread::sleep(POLL_INTERVAL);
    }

    if let Some(p) = target_pid {
        #[cfg(unix)]
        {
            display::error(&format!(
                "Daemon (PID {p}) is stubborn. Force killing (SIGKILL)..."
            ));

            unsafe {
                let _ = libc::kill(p as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    std::thread::sleep(Duration::from_millis(100));

    cleanup_stale_files();

    display::success("Daemon forcefully stopped (SIGKILL).");
}

fn restart_daemon() {
    display::info("Restarting daemon...");

    stop_daemon();

    start_daemon();
}

fn status_daemon() {
    display::info("Checking daemon status...");

    let pid = get_daemon_pid();

    let ipc_ready = daemon_ipc_ready();

    if ipc_ready {
        send_cmd(b"status");

        return;
    }

    if let Some(p) = pid {
        if is_process_alive(p) {
            display::warn(&format!(
                "Daemon process is RUNNING (PID {p}), but IPC is unresponsive/shutting down."
            ));

            return;
        }
    }

    if is_lock_held() {
        display::warn("Daemon lock is HELD by a process, but IPC is unresponsive.");

        return;
    }

    display::warn("Daemon is INACTIVE (Stopped)");
}

fn daemon_ipc_ready() -> bool {
    crate::client::IpcClient::is_ready(CONNECT_TIMEOUT)
}

fn send_cmd(cmd: &[u8]) {
    match crate::client::IpcClient::send_command(cmd, COMMAND_TIMEOUT) {
        Ok(response) => {
            if response.starts_with("OK:") {
                let message = response.strip_prefix("OK:").unwrap_or(&response).trim();
                display::success(message);
            } else if response.starts_with("OK") {
                display::success(&response);
            } else if response.is_empty() {
                display::error("Daemon returned an empty response.");
            } else {
                display::error(&response);
            }
        }
        Err(err) => {
            display::error(&err);
        }
    }
}
