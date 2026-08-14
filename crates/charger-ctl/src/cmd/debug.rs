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
        nodes::{
            detect_node, BATTERY_CAPACITY_NODES, BATTERY_CAPACITY_RAW_NODES, BATTERY_CURRENT_NODES,
            BATTERY_SOC_DECIMAL_NODES, BATTERY_TEMP_NODES, BATTERY_VOLTAGE_NODES, CHARGING_NODES,
            FAST_CHARGE_CURRENT_NODES, INPUT_CURRENT_NODES, SUSPEND_NODES,
            THERMAL_INPUT_CURRENT_NODES,
        },
    },
    error::ChargerError,
};
#[cfg(unix)]
use charger_core::{
    battery::{
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
    logger.log_raw("🎯 HARDWARE ACTUATOR & TELEMETRY CANDIDATES EVALUATION:");
    logger.log_raw("---------------------------------------------------------------");

    let eval_group = |logger: &mut ObserverLogger, label: &str, nodes: &[&str]| {
        logger.log_raw(&format!("• {label}:"));
        for node in nodes {
            let p = Path::new(node);
            if p.exists() {
                let val = fs::read_to_string(p).unwrap_or_else(|_| "<error>".into());
                logger.log_raw(&format!("   [FOUND]    {} = {}", node, val.trim()));
            } else {
                logger.log_raw(&format!("   [MISSING]  {}", node));
            }
        }
    };

    eval_group(&mut logger, "Charging Enabled candidates", CHARGING_NODES);
    eval_group(&mut logger, "Input Suspend candidates", SUSPEND_NODES);
    eval_group(
        &mut logger,
        "Fast Charge Current candidates",
        FAST_CHARGE_CURRENT_NODES,
    );
    eval_group(
        &mut logger,
        "Thermal Input Current candidates",
        THERMAL_INPUT_CURRENT_NODES,
    );
    eval_group(
        &mut logger,
        "Battery SOC Decimal candidates",
        BATTERY_SOC_DECIMAL_NODES,
    );
    eval_group(
        &mut logger,
        "Battery Capacity Raw candidates",
        BATTERY_CAPACITY_RAW_NODES,
    );
    eval_group(
        &mut logger,
        "Battery Capacity candidates",
        BATTERY_CAPACITY_NODES,
    );
    eval_group(
        &mut logger,
        "Battery Voltage candidates",
        BATTERY_VOLTAGE_NODES,
    );
    eval_group(
        &mut logger,
        "Battery Current candidates",
        BATTERY_CURRENT_NODES,
    );
    eval_group(&mut logger, "Input Current candidates", INPUT_CURRENT_NODES);
    eval_group(
        &mut logger,
        "Battery Temperature candidates",
        BATTERY_TEMP_NODES,
    );

    let detected_charging = detect_node(CHARGING_NODES);
    let detected_suspend = detect_node(SUSPEND_NODES);
    let detected_fast_charge = detect_node(FAST_CHARGE_CURRENT_NODES);
    let actual_mode = control::get_actual_charging_state();
    let current_fast_charge = control::read_fast_charge_current();

    logger.log_raw(&format!(
        "\n• Active Charging Node    : {:?}",
        detected_charging
    ));
    logger.log_raw(&format!(
        "• Active Suspend Node     : {:?}",
        detected_suspend
    ));
    logger.log_raw(&format!(
        "• Active Fast Charge Node : {:?}",
        detected_fast_charge
    ));
    logger.log_raw(&format!(
        "• Current Fast Chg Limit  : {:?}",
        current_fast_charge
    ));
    logger.log_raw(&format!("• Actual Hardware State   : {:?}", actual_mode));
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
    max_current: Option<u32>,
    thermal_throttle: Option<bool>,
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
    if let Some(curr) = max_current {
        config.max_charge_current_ma = curr;
    }
    if let Some(therm) = thermal_throttle {
        config.thermal_throttling_enabled = therm;
    }
    config.validate();

    logger.log(&format!(
        "⚙️  SIMULATION CONFIG: charge_limit={}%, resume_limit={}%, max_current={}mA, thermal_throttle={}, poll_interval={}s",
        config.charge_limit, config.resume_limit, config.max_charge_current_ma, config.thermal_throttling_enabled, poll_interval_secs
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
    logger.log("[*] Tekan Ctrl+C untuk berhenti dan melihat ringkasan statistik sesi.\n");

    // Simulasi State Machine
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimState {
        Normal,
        Grace { started_at: Instant },
        Suspended,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimThermalStep {
        Normal,
        Step1, // 2500 mA
        Step2, // 1500 mA
        Step3, // 800 mA
    }

    let mut sim_state = SimState::Normal;
    let mut sim_thermal_step = SimThermalStep::Normal;
    let mut sim_thermal_step_updated_at: Option<Instant> = None;

    let mut last_sample_time = Instant::now() - Duration::from_secs(10);
    let mut uevent_buf = [0u8; 8192];
    let poll_timeout_ms = 1000;

    // Sesi Statistik
    let session_start = Instant::now();
    let mut sample_count: u64 = 0;
    let mut uevent_count: u64 = 0;
    let mut min_soc = f32::MAX;
    let mut max_soc = f32::MIN;
    let mut initial_soc: Option<f32> = None;
    let mut last_soc: Option<f32> = None;
    let mut min_temp = f32::MAX;
    let mut max_temp = f32::MIN;
    let mut min_volt = f32::MAX;
    let mut max_volt = f32::MIN;
    let mut max_charge_current_ma: f32 = 0.0;
    let mut max_charge_wattage_w: f32 = 0.0;
    let mut max_discharge_current_ma: f32 = 0.0;
    let mut fsm_history: Vec<(String, String)> = Vec::new();

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
                uevent_count += 1;

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
            sample_count += 1;

            let power_state = reader::get_power_state().unwrap_or(reader::PowerState::Unknown);
            let capacity_res = reader::read_capacity_raw();
            let temp_res = reader::read_temperature_c();
            let volt_res = reader::read_voltage_uv();
            let input_curr_res = reader::read_input_current_ua();
            let metrics_res = reader::get_battery_metrics();
            let fast_charge_limit_ua = control::read_fast_charge_current();
            let batt_status =
                reader::read_sysfs(Path::new(charger_core::battery::nodes::BATTERY_STATUS_NODE))
                    .unwrap_or_else(|_| "Unknown".into());
            let actual_hw = control::get_actual_charging_state();

            let soc = capacity_res.unwrap_or(0.0);
            let temp_c = temp_res.unwrap_or(0.0);
            let volt_v = volt_res.map(|uv| uv as f32 / 1_000_000.0).unwrap_or(0.0);
            let input_curr_ma = input_curr_res.map(|ua| (ua / 1000) as i32).unwrap_or(0);

            // Statistik tracking
            if initial_soc.is_none() && soc > 0.0 {
                initial_soc = Some(soc);
            }
            if soc > 0.0 {
                min_soc = min_soc.min(soc);
                max_soc = max_soc.max(soc);
                last_soc = Some(soc);
            }
            if temp_c > 0.0 {
                min_temp = min_temp.min(temp_c);
                max_temp = max_temp.max(temp_c);
            }
            if volt_v > 0.0 {
                min_volt = min_volt.min(volt_v);
                max_volt = max_volt.max(volt_v);
            }

            let (batt_curr_str, power_str) = match metrics_res {
                Ok(m) => {
                    if m.is_charging_flow {
                        max_charge_current_ma = max_charge_current_ma.max(m.current_ma);
                        max_charge_wattage_w = max_charge_wattage_w.max(m.wattage_w);
                        (
                            format!("+{:>5.1}mA", m.current_ma),
                            format!("{:>4.2}W", m.wattage_w),
                        )
                    } else {
                        max_discharge_current_ma = max_discharge_current_ma.max(m.current_ma);
                        (
                            format!("-{:>5.1}mA", m.current_ma),
                            format!("{:>4.2}W", m.wattage_w),
                        )
                    }
                }
                Err(_) => ("   ?  mA".to_string(), " ?  W".to_string()),
            };

            // 1. Simulasi Stepped Thermal Throttling
            let is_thermal_emergency = temp_c >= (config.max_temp_dc as f32 / 10.0 + 3.0);
            let desired_thermal_step = if temp_c >= 43.0 {
                SimThermalStep::Step3
            } else if temp_c >= 41.0 {
                SimThermalStep::Step2
            } else if temp_c >= 38.0 {
                SimThermalStep::Step1
            } else {
                SimThermalStep::Normal
            };

            if config.thermal_throttling_enabled && desired_thermal_step != sim_thermal_step {
                let is_step_up = match (desired_thermal_step, sim_thermal_step) {
                    (SimThermalStep::Step3, _) => true,
                    (SimThermalStep::Step2, SimThermalStep::Step1 | SimThermalStep::Normal) => true,
                    (SimThermalStep::Step1, SimThermalStep::Normal) => true,
                    _ => false,
                };
                let hold_expired = sim_thermal_step_updated_at
                    .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(10));

                if is_step_up || hold_expired {
                    sim_thermal_step = desired_thermal_step;
                    sim_thermal_step_updated_at = Some(now);
                }
            }

            let thermal_step_str = match sim_thermal_step {
                SimThermalStep::Normal => "Normal",
                SimThermalStep::Step1 => "Step1(2.5A)",
                SimThermalStep::Step2 => "Step2(1.5A)",
                SimThermalStep::Step3 => "Step3(0.8A)",
            };

            // 2. Transisi State Machine Simulasi
            let is_plugged = power_state.is_plugged_in();

            if !is_plugged {
                if matches!(sim_state, SimState::Grace { .. }) {
                    sim_state = SimState::Normal;
                    let msg =
                        "Charger disconnected -> GRACE CANCELLED (Back to Normal)".to_string();
                    fsm_history.push((chrono_timestamp(), msg.clone()));
                    logger.log(&format!("🔄 [FSM TRANSITION] {msg}"));
                }
                // Suspended dipertahankan saat disconnected (persist)
            } else {
                match sim_state {
                    SimState::Normal => {
                        if soc >= config.charge_limit as f32 {
                            sim_state = SimState::Grace { started_at: now };
                            let msg = format!(
                                "SOC {soc:.2}% >= limit {}% -> Entered GRACE PERIOD (5 mins top-off timer started)",
                                config.charge_limit
                            );
                            fsm_history.push((chrono_timestamp(), msg.clone()));
                            logger.log(&format!("🚨 [FSM TRANSITION] {msg}"));
                        }
                    }
                    SimState::Grace { started_at } => {
                        if soc < config.charge_limit as f32 {
                            sim_state = SimState::Normal;
                            let msg = format!(
                                "SOC dropped to {soc:.2}% < limit {}% -> GRACE CANCELLED (Back to Normal)",
                                config.charge_limit
                            );
                            fsm_history.push((chrono_timestamp(), msg.clone()));
                            logger.log(&format!("🔄 [FSM TRANSITION] {msg}"));
                        } else if now.duration_since(started_at) >= Duration::from_secs(300) {
                            sim_state = SimState::Suspended;
                            let msg = "5-minute Grace period elapsed -> State is now SUSPENDED (Block Charging)".to_string();
                            fsm_history.push((chrono_timestamp(), msg.clone()));
                            logger.log(&format!("⛔ [FSM TRANSITION] {msg}"));
                        }
                    }
                    SimState::Suspended => {
                        if soc <= config.resume_limit as f32 {
                            sim_state = SimState::Normal;
                            let msg = format!(
                                "SOC dropped to {soc:.2}% <= resume {}% -> RESUMED CHARGING (State reset to Normal)",
                                config.resume_limit
                            );
                            fsm_history.push((chrono_timestamp(), msg.clone()));
                            logger.log(&format!("✅ [FSM TRANSITION] {msg}"));
                        }
                    }
                }
            }

            // 3. Target Decision & Current Regulation Simulation
            let (target_decision, sim_current_reg) = if !is_plugged {
                ("NoChange (Disconnected)", "Disabled (0mA)".to_string())
            } else if is_thermal_emergency {
                ("Block (Thermal Emergency)", "Disabled (0mA)".to_string())
            } else {
                match sim_state {
                    SimState::Normal => {
                        let user_limit_ma = config.max_charge_current_ma;
                        let reg_str = match (config.thermal_throttling_enabled, sim_thermal_step) {
                            (true, SimThermalStep::Step3) => {
                                let target = if user_limit_ma > 0 {
                                    user_limit_ma.min(800)
                                } else {
                                    800
                                };
                                format!("ThermalStep3({target}mA)")
                            }
                            (true, SimThermalStep::Step2) => {
                                let target = if user_limit_ma > 0 {
                                    user_limit_ma.min(1500)
                                } else {
                                    1500
                                };
                                format!("ThermalStep2({target}mA)")
                            }
                            (true, SimThermalStep::Step1) => {
                                let target = if user_limit_ma > 0 {
                                    user_limit_ma.min(2500)
                                } else {
                                    2500
                                };
                                format!("ThermalStep1({target}mA)")
                            }
                            _ => {
                                if user_limit_ma > 0 {
                                    format!("UserLimit({user_limit_ma}mA)")
                                } else {
                                    "Unconstrained (Full)".to_string()
                                }
                            }
                        };
                        ("Allow (Normal Charging)", reg_str)
                    }
                    SimState::Grace { .. } => {
                        let user_limit_ma = config.max_charge_current_ma;
                        let target = if user_limit_ma > 0 {
                            user_limit_ma.min(1000)
                        } else {
                            1000
                        };
                        ("Allow (Grace Top-Off)", format!("GraceCap({target}mA)"))
                    }
                    SimState::Suspended => ("Block (Charge Limit)", "Disabled (0mA)".to_string()),
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

            let fast_chg_node_str = match fast_charge_limit_ua {
                Some(ua) => format!("{}mA", ua / 1000),
                None => "N/A".to_string(),
            };

            logger.log(&format!(
                "📊 SOC: {soc:>5.2}% | Temp: {temp_c:>4.1}°C ({thermal_step_str:<12}) | Volt: {volt_v:>4.2}V | Batt: {batt_curr_str} ({power_str}) | InCurr: {input_curr_ma:>4}mA | FastChgNode: {fast_chg_node_str:<6} | Power: {power_state:?} | Status: {batt_status:<10} | FSM: {fsm_str:<17} | Decision: {target_decision:<23} | Reg: {sim_current_reg:<22} | ActualHW: {actual_hw:?}"
            ));
        }
    }

    unsafe { libc::close(fd) };

    // Ringkasan Statistik Akhir
    let duration_secs = session_start.elapsed().as_secs();
    let duration_min = duration_secs / 60;
    let duration_rem_sec = duration_secs % 60;

    logger.log_raw("\n===============================================================");
    logger.log_raw("       📊 CHARGER-CONTROL: OBSERVATION SESSION SUMMARY         ");
    logger.log_raw("===============================================================");
    logger.log_raw(&format!(
        "• Total Duration          : {}m {}s ({} seconds)",
        duration_min, duration_rem_sec, duration_secs
    ));
    logger.log_raw(&format!("• Total Sensor Samples    : {}", sample_count));
    logger.log_raw(&format!("• Total Kernel Uevents    : {}", uevent_count));

    if let (Some(init), Some(last)) = (initial_soc, last_soc) {
        let delta = last - init;
        let delta_sign = if delta >= 0.0 { "+" } else { "" };
        logger.log_raw(&format!("• SOC Profile             : Start: {init:.2}% -> End: {last:.2}% (Delta: {delta_sign}{delta:.2}%)"));
        logger.log_raw(&format!(
            "• SOC Observed Range      : Min: {min_soc:.2}% | Max: {max_soc:.2}%"
        ));
    }

    if min_temp <= max_temp {
        logger.log_raw(&format!(
            "• Temperature Range       : Min: {min_temp:.1}°C | Max: {max_temp:.1}°C"
        ));
    }

    if min_volt <= max_volt {
        logger.log_raw(&format!(
            "• Voltage Range           : Min: {min_volt:.2}V | Max: {max_volt:.2}V"
        ));
    }

    logger.log_raw(&format!(
        "• Max Charging Current    : +{max_charge_current_ma:.1} mA ({max_charge_wattage_w:.2} W)"
    ));
    logger.log_raw(&format!(
        "• Max Discharging Current : -{max_discharge_current_ma:.1} mA"
    ));

    logger.log_raw(&format!(
        "\n• FSM Transition History  : Total {} events",
        fsm_history.len()
    ));
    if fsm_history.is_empty() {
        logger.log_raw("   (Tidak ada transisi state FSM selama masa observasi)");
    } else {
        for (ts, desc) in &fsm_history {
            logger.log_raw(&format!("   ├─ [{ts}] {desc}"));
        }
    }

    logger.log_raw("===============================================================");
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
    _max_current: Option<u32>,
    _thermal_throttle: Option<bool>,
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
