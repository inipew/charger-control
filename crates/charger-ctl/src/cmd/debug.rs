use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use charger_core::{
    battery::{
        control,
        nodes::{detect_node, CHARGING_NODES, SUSPEND_NODES},
    },
    error::ChargerError,
};
#[cfg(unix)]
use charger_core::{
    battery::{
        control::ActualHardwareMode,
        reader,
        uevent::{classify_uevent, parse_uevent_properties, UeventKind},
    },
    config::schema::Config,
};

/// Dual logger yang menulis ke stdout dan file secara bersamaan.
struct ObserverLogger {
    file: Option<File>,
}

impl ObserverLogger {
    fn new(file_path: Option<&Path>) -> Result<Self, ChargerError> {
        let file = if let Some(path) = file_path {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let f = OpenOptions::new()
                .create(true)
                .write(true)
                .append(true)
                .open(path)
                .map_err(|e| ChargerError::ConfigWrite {
                    path: path.to_path_buf(),
                    source: e,
                })?;
            Some(f)
        } else {
            None
        };
        Ok(Self { file })
    }

    fn log(&mut self, msg: &str) {
        let timestamp = chrono_timestamp();
        let formatted = format!("[{timestamp}] {msg}");
        println!("{formatted}");
        if let Some(ref mut f) = self.file {
            let _ = writeln!(f, "{formatted}");
            let _ = f.flush();
        }
    }

    fn log_raw(&mut self, msg: &str) {
        println!("{msg}");
        if let Some(ref mut f) = self.file {
            let _ = writeln!(f, "{msg}");
            let _ = f.flush();
        }
    }
}

fn chrono_timestamp() -> String {
    #[cfg(unix)]
    unsafe {
        let mut tv: libc::timeval = std::mem::zeroed();
        libc::gettimeofday(&mut tv, std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&tv.tv_sec, &mut tm);
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
            tv.tv_usec / 1000
        )
    }
    #[cfg(not(unix))]
    {
        "00:00:00.000".to_string()
    }
}

/// Snapshot & Scan mendalam terhadap seluruh node sysfs power_supply pada perangkat.
pub fn run_node_dump(output_file: Option<&Path>) -> Result<(), ChargerError> {
    let mut logger = ObserverLogger::new(output_file)?;

    logger.log_raw("===============================================================");
    logger.log_raw("       CHARGER-CONTROL: DEEP HARDWARE & SYSFS NODE PROBE       ");
    logger.log_raw("===============================================================");

    let ps_dir = Path::new("/sys/class/power_supply");
    if !ps_dir.exists() {
        logger.log("[-] /sys/class/power_supply tidak ditemukan pada platform ini.");
        return Ok(());
    }

    let entries = fs::read_dir(ps_dir).map_err(|e| ChargerError::SysfsRead {
        path: ps_dir.to_path_buf(),
        source: e,
    })?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            logger.log_raw(&format!(
                "\n📂 POWER SUPPLY DEVICE: [{name}] ({})",
                path.display()
            ));

            if let Ok(files) = fs::read_dir(&path) {
                let mut sorted_files: Vec<_> = files.flatten().collect();
                sorted_files.sort_by_key(|f| f.file_name());

                for file in sorted_files {
                    let file_path = file.path();
                    if file_path.is_file() {
                        let fname = file_path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("?");
                        let val_str = fs::read_to_string(&file_path)
                            .map(|s| s.trim().to_string())
                            .unwrap_or_else(|_| "<unreadable>".to_string());

                        let perms_str = if let Ok(_meta) = fs::metadata(&file_path) {
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                format!("{:04o}", _meta.permissions().mode() & 0o777)
                            }
                            #[cfg(not(unix))]
                            {
                                "----".to_string()
                            }
                        } else {
                            "????".to_string()
                        };

                        logger.log_raw(&format!("   ├─ {fname:<28} [{perms_str}] = {val_str}"));
                    }
                }
            }
        }
    }

    logger.log_raw("\n---------------------------------------------------------------");
    logger.log_raw("🎯 CHARGING CONTROL CANDIDATE NODES EVALUATION:");
    logger.log_raw("---------------------------------------------------------------");

    logger.log("• Charging Enabled candidates:");
    for node in CHARGING_NODES {
        let p = Path::new(node);
        if p.exists() {
            let val = fs::read_to_string(p).unwrap_or_else(|_| "<error>".into());
            logger.log_raw(&format!("   [FOUND]    {} = {}", node, val.trim()));
        } else {
            logger.log_raw(&format!("   [MISSING]  {}", node));
        }
    }

    logger.log("• Input Suspend candidates:");
    for node in SUSPEND_NODES {
        let p = Path::new(node);
        if p.exists() {
            let val = fs::read_to_string(p).unwrap_or_else(|_| "<error>".into());
            logger.log_raw(&format!("   [FOUND]    {} = {}", node, val.trim()));
        } else {
            logger.log_raw(&format!("   [MISSING]  {}", node));
        }
    }

    let detected_charging = detect_node(CHARGING_NODES);
    let detected_suspend = detect_node(SUSPEND_NODES);
    let actual_mode = control::get_actual_charging_state();

    logger.log_raw(&format!(
        "\n• Active Charging Node : {:?}",
        detected_charging
    ));
    logger.log_raw(&format!("• Active Suspend Node  : {:?}", detected_suspend));
    logger.log_raw(&format!("• Actual Hardware State: {:?}", actual_mode));
    logger.log_raw("===============================================================\n");

    Ok(())
}

/// Mode Simulasi & Observasi Real-Time (Safe / Dry-Run / Read-Only).
/// Daemon logika berjalan penuh, tapi TIDAK PERNAH menulis ke sysfs actuator.
#[cfg(unix)]
pub fn run_observer(
    output_file: Option<PathBuf>,
    poll_interval_secs: u64,
    charge_limit: Option<u8>,
    resume_limit: Option<u8>,
) -> Result<(), ChargerError> {
    let mut logger = ObserverLogger::new(output_file.as_deref())?;

    logger.log_raw("===============================================================");
    logger.log_raw("       CHARGER-CONTROL: REAL-TIME OBSERVER & DRY-RUN MODE       ");
    logger.log_raw("  [SAFE READ-ONLY] Sysfs actuator writes are completely disabled ");
    logger.log_raw("===============================================================");

    // 1. Initial Node Scan
    run_node_dump(output_file.as_deref())?;

    // 2. Setup Configuration
    let mut config = Config::default();
    if let Some(limit) = charge_limit {
        config.charge_limit = limit;
    }
    if let Some(resume) = resume_limit {
        config.resume_limit = resume;
    }
    config.validate();

    logger.log(&format!(
        "⚙️  SIMULATION CONFIG: charge_limit={}%, resume_limit={}%, poll_interval={}s",
        config.charge_limit, config.resume_limit, poll_interval_secs
    ));
    logger.log("[*] Setting up Netlink uevent broadcast socket...");

    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(ChargerError::ParseError("Failed creating netlink socket"));
    }

    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_pid = std::process::id() as u32;
    addr.nl_groups = 1;

    let ret = unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    };
    if ret < 0 {
        unsafe { libc::close(fd) };
        return Err(ChargerError::ParseError(
            "Failed binding netlink socket (run as root / su)",
        ));
    }

    // Handle Ctrl+C gracefully
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    let _ = ctrlc_handler(move || {
        r.store(false, Ordering::SeqCst);
    });

    logger
        .log("[*] Live monitoring started! Silakan colok / cabut charger, atau biarkan charging.");
    logger.log("[*] Tekan Ctrl+C untuk berhenti dan menyimpan log.\n");

    // Simulasi State Machine
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimState {
        Normal,
        Grace { started_at: Instant },
        Suspended,
    }

    let mut sim_state = SimState::Normal;
    let mut last_sample_time = Instant::now() - Duration::from_secs(10);
    let mut uevent_buf = [0u8; 8192];

    let poll_timeout_ms = 1000; // 1s sub-poll for responsive event loop

    while running.load(Ordering::SeqCst) {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };

        let poll_res = unsafe { libc::poll(&mut pfd, 1, poll_timeout_ms) };
        let now = Instant::now();

        if poll_res > 0 && (pfd.revents & libc::POLLIN != 0) {
            let len = unsafe {
                libc::recv(
                    fd,
                    uevent_buf.as_mut_ptr() as *mut libc::c_void,
                    uevent_buf.len(),
                    0,
                )
            };

            if len > 0 {
                let raw_bytes = &uevent_buf[..len as usize];
                let kind = classify_uevent(raw_bytes);

                if kind != UeventKind::Other {
                    let props = parse_uevent_properties(raw_bytes);
                    logger.log_raw(&format!(
                        "\n⚡ [{}] KERNEL UEVENT RECEIVED: Classified as {:?}",
                        chrono_timestamp(),
                        kind
                    ));
                    for (k, v) in &props {
                        logger.log_raw(&format!("   • {k} = {v}"));
                    }
                }
            }
        }

        // Periodic Battery Reading & Policy Evaluation
        if now.duration_since(last_sample_time) >= Duration::from_secs(poll_interval_secs) {
            last_sample_time = now;

            let power_state = reader::get_power_state().unwrap_or(reader::PowerState::Unknown);
            let capacity_res = reader::read_capacity_raw();
            let temp_res = reader::read_temperature_c();
            let volt_res = reader::read_voltage_uv();
            let curr_res = reader::read_battery_current_ua();
            let input_curr_res = reader::read_input_current_ua();
            let batt_status =
                reader::read_sysfs(Path::new(charger_core::battery::nodes::BATTERY_STATUS_NODE))
                    .unwrap_or_else(|_| "Unknown".into());
            let actual_hw = control::get_actual_charging_state();

            let soc = capacity_res.unwrap_or(0.0);
            let temp_c = temp_res.unwrap_or(0.0);
            let volt_v = volt_res.map(|uv| uv as f32 / 1_000_000.0).unwrap_or(0.0);
            let curr_ma = curr_res.map(|ua| (ua / 1000) as i32).unwrap_or(0);
            let input_curr_ma = input_curr_res.map(|ua| (ua / 1000) as i32).unwrap_or(0);

            // Transisi State Machine Simulasi
            let is_plugged = power_state.is_plugged_in();

            if !is_plugged {
                if matches!(sim_state, SimState::Grace { .. }) {
                    sim_state = SimState::Normal;
                }
                // Suspended dipertahankan saat disconnected (persist)
            } else {
                match sim_state {
                    SimState::Normal => {
                        if soc >= config.charge_limit as f32 {
                            sim_state = SimState::Grace { started_at: now };
                            logger.log(&format!(
                                "🚨 [FSM TRANSITION] SOC {soc:.1}% >= limit {}% -> Entered GRACE PERIOD (5 mins top-off timer started)",
                                config.charge_limit
                            ));
                        }
                    }
                    SimState::Grace { started_at } => {
                        if soc < config.charge_limit as f32 {
                            sim_state = SimState::Normal;
                            logger.log(&format!(
                                "🔄 [FSM TRANSITION] SOC dropped to {soc:.1}% < limit {}% -> GRACE CANCELLED (Back to Normal)",
                                config.charge_limit
                            ));
                        } else if now.duration_since(started_at) >= Duration::from_secs(300) {
                            sim_state = SimState::Suspended;
                            logger.log(&format!(
                                "⛔ [FSM TRANSITION] 5-minute Grace period elapsed -> State is now SUSPENDED (Block Charging)"
                            ));
                        }
                    }
                    SimState::Suspended => {
                        if soc <= config.resume_limit as f32 {
                            sim_state = SimState::Normal;
                            logger.log(&format!(
                                "✅ [FSM TRANSITION] SOC dropped to {soc:.1}% <= resume {}% -> RESUMED CHARGING (State reset to Normal)",
                                config.resume_limit
                            ));
                        }
                    }
                }
            }

            // Target Decision Simulation
            let target_decision = if !is_plugged {
                "NoChange (Disconnected)"
            } else {
                match sim_state {
                    SimState::Normal => "Allow (Normal Charging)",
                    SimState::Grace { .. } => "Allow (Grace Top-Off)",
                    SimState::Suspended => "Block (Charge Limit Hysteresis)",
                }
            };

            let fsm_str = match sim_state {
                SimState::Normal => "Normal".to_string(),
                SimState::Grace { started_at } => {
                    let elapsed = now.duration_since(started_at).as_secs();
                    format!("Grace({elapsed}s/300s)")
                }
                SimState::Suspended => "Suspended [BLOCK]".to_string(),
            };

            logger.log(&format!(
                "📊 SOC: {soc:>5.1}% | Temp: {temp_c:>4.1}°C | Volt: {volt_v:>4.2}V | Curr: {curr_ma:>5}mA | InCurr: {input_curr_ma:>4}mA | Power: {power_state:?} | Status: {batt_status:<10} | FSM: {fsm_str:<18} | Target: {target_decision:<25} | ActualHW: {actual_hw:?}"
            ));
        }
    }

    unsafe { libc::close(fd) };
    logger.log_raw("\n===============================================================");
    logger.log("🛑 Observer stopped. Log saved successfully.");
    logger.log_raw("===============================================================");

    Ok(())
}

#[cfg(not(unix))]
pub fn run_observer(
    _output_file: Option<PathBuf>,
    _poll_interval_secs: u64,
    _charge_limit: Option<u8>,
    _resume_limit: Option<u8>,
) -> Result<(), ChargerError> {
    println!("Observer is only supported on Linux/Android.");
    Ok(())
}

#[cfg(unix)]
fn ctrlc_handler<F>(handler: F) -> Result<(), ChargerError>
where
    F: Fn() + Send + 'static,
{
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([
        signal_hook::consts::signal::SIGINT,
        signal_hook::consts::signal::SIGTERM,
    ]) {
        std::thread::spawn(move || {
            if signals.forever().next().is_some() {
                handler();
            }
        });
    }
    Ok(())
}

#[cfg(unix)]
pub fn run_uevent_dumper() -> Result<(), ChargerError> {
    println!("=== UEVENT DUMPER ===");
    println!("Listening for netlink broadcast (uevent) messages...");
    println!("Please plug or unplug your charger to see hardware events.");
    println!("Press Ctrl+C to stop.\n");

    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(ChargerError::ParseError("Failed to create netlink socket"));
    }

    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_pid = std::process::id() as u32;
    addr.nl_groups = 1;

    let ret = unsafe {
        libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    };

    if ret < 0 {
        unsafe { libc::close(fd) };
        return Err(ChargerError::ParseError(
            "Failed to bind netlink socket (run as root?)",
        ));
    }

    let mut buf = [0u8; 8192];
    loop {
        let res = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if res > 0 {
            let data = &buf[..res as usize];
            let s = String::from_utf8_lossy(data);

            if s.contains("power_supply") || s.contains("battery") || s.contains("typec") {
                let parts: Vec<&str> = s.split('\0').collect();
                println!("--- UEVENT KERNEL BROADCAST ---");
                for part in parts {
                    if !part.is_empty() {
                        println!("  {}", part);
                    }
                }
                println!("-------------------------------\n");
            }
        }
    }
}

#[cfg(not(unix))]
pub fn run_uevent_dumper() -> Result<(), ChargerError> {
    println!("Netlink uevent dumper is only supported on Linux/Android.");
    Ok(())
}
