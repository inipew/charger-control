#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentUnit {
    MicroAmp,
    MilliAmp,
}

/// Peran semantik dari node arus — battery current vs input/charger current.
/// Keduanya mengukur hal yang berbeda dan tidak boleh dipertukarkan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentRole {
    /// Arus baterai aktual. Sign bersifat vendor-specific (negatif bisa berarti charging).
    Battery,
    /// Arus yang ditarik dari sumber daya eksternal (charger/USB).
    Input,
}

#[derive(Debug, Clone, Copy)]
pub struct CurrentNodeConfig {
    pub path: &'static str,
    pub unit: CurrentUnit,
    pub priority: u8,
    /// Peran semantik node ini — Battery atau Input.
    pub role: CurrentRole,
}

/// Konfigurasi bagaimana presence charger ditentukan untuk profile ini.
///
/// Pada device yang tidak memiliki online node (misalnya hanya punya main/current_now),
/// `ChargerPresence::Online` berarti **"arus input sedang aktif"** (InputActivity),
/// bukan physical cable presence.
///
/// Semua threshold dalam **mA** agar konsisten dengan unit internal domain charger-core.
#[derive(Debug, Clone, Copy)]
pub struct PresenceProfile {
    /// Threshold atas (mA). Jika input_current_ma >= nilai ini → Online langsung.
    /// None = tidak gunakan input current sebagai sinyal presence.
    pub input_online_threshold_ma: Option<i32>,
    /// Threshold bawah (mA). Jika input_current_ma <= nilai ini → kandidat Offline.
    /// None = tidak gunakan input current sebagai sinyal presence.
    pub input_offline_threshold_ma: Option<i32>,
    /// Node sysfs berbasis "1"/"0" (mis. usb/online, ac/online).
    /// Jika ada yang terbaca, mendapat prioritas LEBIH TINGGI dari input_current.
    pub online_nodes: &'static [&'static str],
}

#[derive(Debug, Clone, Copy)]
pub struct ControlProfile {
    pub charging_nodes: &'static [&'static str],
    pub suspend_nodes: &'static [&'static str],
}

#[derive(Debug, Clone, Copy)]
pub struct SensorProfile {
    pub current_nodes: &'static [CurrentNodeConfig],
    /// Konfigurasi deteksi presence charger.
    pub presence: PresenceProfile,
    pub capacity_path: &'static str,
    pub temperature_path: &'static str,
    pub status_path: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct CapabilityProfile {
    pub supports_charging_toggle: bool,
    pub supports_input_suspend: bool,
    pub supports_current_measurement: bool,
    pub supports_temperature: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct HardwareProfile {
    pub name: &'static str,
    pub control: ControlProfile,
    pub sensor: SensorProfile,
    pub capabilities: CapabilityProfile,
}

/// Profile generik — mencoba berbagai node umum; cocok untuk device tidak dikenal.
pub const GENERIC_PROFILE: HardwareProfile = HardwareProfile {
    name: "generic",
    control: ControlProfile {
        charging_nodes: &[
            "/sys/class/power_supply/battery/charging_enabled",
            "/sys/class/power_supply/main/charging_enabled",
            "/sys/class/power_supply/battery/battery_charging_enabled",
        ],
        suspend_nodes: &[
            "/sys/class/power_supply/battery/input_suspend",
            "/sys/class/power_supply/usb/input_suspend",
        ],
    },
    sensor: SensorProfile {
        current_nodes: &[
            CurrentNodeConfig { path: "/sys/class/power_supply/battery/current_now",
                unit: CurrentUnit::MicroAmp, priority: 100, role: CurrentRole::Battery },
            CurrentNodeConfig { path: "/sys/class/power_supply/bms/current_now",
                unit: CurrentUnit::MicroAmp, priority: 90, role: CurrentRole::Battery },
            CurrentNodeConfig { path: "/sys/class/power_supply/main/current_now",
                unit: CurrentUnit::MicroAmp, priority: 80, role: CurrentRole::Input },
            CurrentNodeConfig { path: "/sys/class/power_supply/battery/batt_current_now",
                unit: CurrentUnit::MicroAmp, priority: 70, role: CurrentRole::Battery },
            CurrentNodeConfig { path: "/sys/class/power_supply/usb/current_now",
                unit: CurrentUnit::MicroAmp, priority: 60, role: CurrentRole::Input },
        ],
        presence: PresenceProfile {
            input_online_threshold_ma: Some(100),
            input_offline_threshold_ma: Some(50),
            online_nodes: &[
                "/sys/class/power_supply/usb/online",
                "/sys/class/power_supply/ac/online",
                "/sys/class/power_supply/wireless/online",
                "/sys/class/power_supply/dc/online",
            ],
        },
        capacity_path: "/sys/class/power_supply/battery/capacity",
        temperature_path: "/sys/class/power_supply/battery/temp",
        status_path: "/sys/class/power_supply/battery/status",
    },
    capabilities: CapabilityProfile {
        supports_charging_toggle: true,
        supports_input_suspend: true,
        supports_current_measurement: true,
        supports_temperature: true,
    },
};

/// Profile untuk device Android dengan main/current_now sebagai sinyal input,
/// tanpa online node yang tersedia.
///
/// CATATAN: `ChargerPresence::Online` pada profile ini berarti **arus input aktif**
/// (InputActivity), bukan physical cable presence. Ketika daemon mematikan charging,
/// main/current_now turun ke 0 dan presence menjadi `Unknown` — bukan `Offline`.
pub const DEVICE_PROFILE: HardwareProfile = HardwareProfile {
    name: "android-typec-main",
    control: ControlProfile {
        charging_nodes: &[
            "/sys/class/power_supply/battery/charging_enabled",
        ],
        suspend_nodes: &[
            "/sys/class/power_supply/battery/input_suspend",
        ],
    },
    sensor: SensorProfile {
        current_nodes: &[
            CurrentNodeConfig {
                path: "/sys/class/power_supply/battery/current_now",
                unit: CurrentUnit::MicroAmp,
                priority: 100,
                role: CurrentRole::Battery,
            },
            CurrentNodeConfig {
                path: "/sys/class/power_supply/main/current_now",
                unit: CurrentUnit::MicroAmp,
                priority: 100,
                role: CurrentRole::Input,
            },
        ],
        presence: PresenceProfile {
            input_online_threshold_ma: Some(100),
            input_offline_threshold_ma: Some(50),
            online_nodes: &[], // device ini tidak memiliki online node
        },
        capacity_path: "/sys/class/power_supply/battery/capacity",
        temperature_path: "/sys/class/power_supply/battery/temp",
        status_path: "/sys/class/power_supply/battery/status",
    },
    capabilities: CapabilityProfile {
        supports_charging_toggle: true,
        supports_input_suspend: true,
        supports_current_measurement: true,
        supports_temperature: true,
    },
};
