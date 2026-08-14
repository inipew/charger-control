/// Klasifikasi jenis uevent kernel yang relevan untuk power supply & battery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UeventKind {
    Ac,
    Usb,
    TypeC,
    Battery,
    Bms,
    Other,
}

/// Klasifikasi payload kernel uevent (NETLINK_KOBJECT_UEVENT).
pub fn classify_uevent(data: &[u8]) -> UeventKind {
    let mut subsystem: Option<&[u8]> = None;
    let mut devpath: Option<&[u8]> = None;
    let mut power_supply_name: Option<&[u8]> = None;

    for part in data.split(|&b| b == 0) {
        if part.starts_with(b"SUBSYSTEM=") {
            subsystem = Some(&part[10..]);
        } else if part.starts_with(b"DEVPATH=") {
            devpath = Some(&part[8..]);
        } else if part.starts_with(b"POWER_SUPPLY_NAME=") {
            power_supply_name = Some(&part[18..]);
        }
    }

    if subsystem == Some(b"typec")
        || devpath.is_some_and(|dp| dp.windows(6).any(|w| w == b"/typec"))
    {
        return UeventKind::TypeC;
    }

    if subsystem == Some(b"power_supply") {
        if let Some(name) = power_supply_name {
            match name {
                b"ac" | b"main" | b"mains" | b"wireless" => return UeventKind::Ac,
                b"usb" | b"charger" => return UeventKind::Usb,
                b"typec" => return UeventKind::TypeC,
                b"battery" => return UeventKind::Battery,
                b"bms" => return UeventKind::Bms,
                _ => {}
            }
        }
    }

    if let Some(dp) = devpath {
        if dp.windows(4).any(|w| w == b"/bms") || dp.ends_with(b"/bms") {
            return UeventKind::Bms;
        }
        if dp.windows(8).any(|w| w == b"/battery") || dp.ends_with(b"/battery") {
            return UeventKind::Battery;
        }
        if dp.windows(8).any(|w| w == b"/charger") || dp.ends_with(b"/charger") {
            return UeventKind::Usb;
        }
        if dp.windows(4).any(|w| w == b"/usb") || dp.ends_with(b"/usb") {
            return UeventKind::Usb;
        }
        if dp.windows(5).any(|w| w == b"/main") || dp.ends_with(b"/main") {
            return UeventKind::Ac;
        }
        if dp.windows(3).any(|w| w == b"/ac") || dp.ends_with(b"/ac") {
            return UeventKind::Ac;
        }
    }

    if let Some(name) = power_supply_name {
        if name.starts_with(b"battery") {
            return UeventKind::Battery;
        }
        if name.starts_with(b"bms") {
            return UeventKind::Bms;
        }
    }

    UeventKind::Other
}

/// Parse pasangan key=value dari format buffer uevent null-delimited.
pub fn parse_uevent_properties(data: &[u8]) -> Vec<(String, String)> {
    let mut props = Vec::new();
    for part in data.split(|&b| b == 0) {
        if part.is_empty() {
            continue;
        }
        if let Ok(s) = std::str::from_utf8(part) {
            if let Some((k, v)) = s.split_once('=') {
                props.push((k.to_string(), v.to_string()));
            } else {
                props.push(("RAW".to_string(), s.to_string()));
            }
        }
    }
    props
}
