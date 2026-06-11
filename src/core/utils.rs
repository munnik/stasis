// Author: Dustin Pilgrim
// License: GPL-3.0-only

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0));
    d.as_millis() as u64
}

// ---------------- power / chassis ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChassisKind {
    Laptop,
    Desktop,
}

pub fn detect_chassis() -> ChassisKind {
    if let Ok(data) = std::fs::read_to_string("/sys/class/dmi/id/chassis_type") {
        match data.trim() {
            // Portable / Laptop / Notebook / Convertible / Tablet
            "8" | "9" | "10" | "14" | "30" | "31" | "32" => {
                return ChassisKind::Laptop;
            }
            _ => {}
        }
    }

    ChassisKind::Desktop
}

pub fn is_laptop() -> bool {
    matches!(detect_chassis(), ChassisKind::Laptop)
}

pub fn is_on_ac_power() -> bool {
    if let Ok(entries) = std::fs::read_dir("/sys/class/power_supply/") {
        for entry in entries.flatten() {
            let path = entry.path();

            // Standard sysfs way
            if let Ok(typ) = std::fs::read_to_string(path.join("type")) {
                if typ.trim() == "Mains" {
                    if let Ok(online) = std::fs::read_to_string(path.join("online")) {
                        if online.trim() == "1" {
                            return true;
                        }
                    }
                }
            }

            // Legacy fallbacks
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if ["AC", "AC0", "ADP", "ADP0", "ACAD"]
                    .iter()
                    .any(|p| name.starts_with(p))
                {
                    if let Ok(online) = std::fs::read_to_string(path.join("online")) {
                        if online.trim() == "1" {
                            return true;
                        }
                    }
                }
            }
        }
    }

    false
}
