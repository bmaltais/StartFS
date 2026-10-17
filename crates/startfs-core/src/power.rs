//! Whether the machine runs on battery. Injectable so tests can unplug it.

use std::sync::atomic::{AtomicBool, Ordering};

pub trait PowerSource: Send + Sync {
    /// True while the machine runs on battery power.
    fn on_battery(&self) -> bool;
}

/// A machine that is always plugged in.
pub struct MainsPower;

impl PowerSource for MainsPower {
    fn on_battery(&self) -> bool {
        false
    }
}

/// Asks the operating system. When it cannot tell, the machine counts as plugged in.
pub struct SystemPower;

impl PowerSource for SystemPower {
    fn on_battery(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_dir("/sys/class/power_supply")
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("BAT"))
                .any(|e| {
                    std::fs::read_to_string(e.path().join("status"))
                        .is_ok_and(|s| s.trim() == "Discharging")
                })
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("pmset")
                .args(["-g", "batt"])
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("Battery Power"))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            false
        }
    }
}

/// Power that only changes when told to.
#[derive(Debug, Default)]
pub struct ManualPower(AtomicBool);

impl ManualPower {
    pub fn set_on_battery(&self, on: bool) {
        self.0.store(on, Ordering::SeqCst);
    }
}

impl PowerSource for ManualPower {
    fn on_battery(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
