// Author: Dustin Pilgrim
// License: GPL-3.0-only

//! Conservative hardware power-down for the low-power idle phase.
//!
//! When the idle plan's DPMS step has fired and the configured timeout elapses,
//! Stasis enters low-power mode: it snapshots the current state of supported
//! hardware knobs, writes conservative power-saving values, and on any resume
//! path restores exactly what it changed from the snapshot.
//!
//! Currently supported (all best-effort, permission-gated):
//! - PCI GPU devices (VGA / 3D controllers): `power/control` -> `auto`
//!   (enables runtime PM so the device can sleep while the display is off).
//! - AMDGPU cards: additionally `power_dpm_force_performance_level` -> `auto`
//!   (lets the driver scale clocks down).
//!
//! Writes that fail (e.g. permission denied without elevated privileges or a
//! udev rule) are skipped and logged; only successfully-changed files are
//! recorded for restore.

use std::fs;
use std::path::{Path, PathBuf};

/// One snapshot entry: the sysfs path and the exact value it held before we
/// touched it. Restore writes this value back verbatim.
#[derive(Debug, Clone)]
struct SnapEntry {
    path: PathBuf,
    original: String,
}

#[derive(Debug, Default)]
pub struct LowPowerController {
    /// Entries we successfully read AND successfully overwrote.
    /// Only these are restored on exit.
    snapshot: Vec<SnapEntry>,
    active: bool,
}

impl LowPowerController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Scan for supported hardware, snapshot current values, and apply
    /// conservative power-saving settings. Returns the number of knobs changed.
    #[allow(clippy::collapsible_if)]
    pub fn enter(&mut self) -> usize {
        if self.active {
            eventline::debug!("low_power: already active, skipping enter");
            return 0;
        }

        let mut changed = 0usize;
        let mut new_snapshot: Vec<SnapEntry> = Vec::new();

        for dev in discover_gpu_pci_devices() {
            // Runtime PM: power/control -> auto
            if snapshot_and_write(&dev.power_control, "auto", &mut new_snapshot).is_some() {
                changed += 1;
            }

            // AMDGPU DPM: power_dpm_force_performance_level -> auto
            if let Some(dpm) = &dev.power_dpm_force_performance_level {
                if snapshot_and_write(dpm, "auto", &mut new_snapshot).is_some() {
                    changed += 1;
                }
            }
        }

        self.snapshot = new_snapshot;
        self.active = true;

        if changed > 0 {
            eventline::info!("low_power: entered ({} knob(s) applied)", changed);
        } else {
            eventline::info!(
                "low_power: entered but no writable knobs found (may need elevated privileges or a udev rule)"
            );
        }

        changed
    }

    /// Restore every snapshotted value verbatim. Returns the number restored.
    pub fn exit(&mut self) -> usize {
        if !self.active {
            return 0;
        }

        let mut restored = 0usize;

        for entry in self.snapshot.drain(..) {
            match fs::write(&entry.path, &entry.original) {
                Ok(()) => {
                    restored += 1;
                    eventline::debug!(
                        "low_power: restored {} -> {}",
                        entry.path.display(),
                        entry.original.trim()
                    );
                }
                Err(e) => {
                    // File may have disappeared, or permissions changed. Keep
                    // trying the rest rather than leaving other devices stuck.
                    eventline::warn!("low_power: failed to restore {}: {e}", entry.path.display());
                }
            }
        }

        self.active = false;

        if restored > 0 {
            eventline::info!("low_power: exited ({} knob(s) restored)", restored);
        } else {
            eventline::info!("low_power: exited");
        }

        restored
    }
}

impl Drop for LowPowerController {
    fn drop(&mut self) {
        // Safety net: if the daemon shuts down while low-power is active,
        // restore the hardware so the user is never left in a reduced state.
        if self.active {
            eventline::warn!("low_power: daemon shutting down while active; restoring hardware");
            let _ = self.exit();
        }
    }
}

// ---------------- helpers ----------------

/// Read a file's current value. Returns None if unreadable.
fn read_sysfs(path: &Path) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) => {
            eventline::debug!("low_power: could not read {}: {e}", path.display());
            None
        }
    }
}

/// Snapshot the current value of `path`, then write `new_value`.
/// On success, pushes a SnapEntry and returns Some(()).
/// On any failure (read or write), pushes nothing and returns None.
fn snapshot_and_write(path: &Path, new_value: &str, snapshot: &mut Vec<SnapEntry>) -> Option<()> {
    let original = read_sysfs(path)?;

    // Nothing to do if it's already at the target value — avoids recording
    // a no-op restore and keeps the snapshot minimal.
    if original.trim() == new_value {
        return None;
    }

    match fs::write(path, new_value) {
        Ok(()) => {
            snapshot.push(SnapEntry {
                path: path.to_path_buf(),
                original,
            });
            eventline::debug!(
                "low_power: {} -> {} (was {})",
                path.display(),
                new_value,
                snapshot
                    .last()
                    .map(|e| e.original.trim().to_string())
                    .unwrap_or_default()
            );
            Some(())
        }
        Err(e) => {
            eventline::debug!(
                "low_power: could not write {} (likely needs elevated privileges): {e}",
                path.display()
            );
            None
        }
    }
}

/// A discovered GPU PCI device with the knobs we care about.
struct GpuDevice {
    power_control: PathBuf,
    power_dpm_force_performance_level: Option<PathBuf>,
}

/// Discover PCI devices whose class is a display controller (0x03xxxx).
fn discover_gpu_pci_devices() -> Vec<GpuDevice> {
    let mut out = Vec::new();

    let entries = match fs::read_dir("/sys/bus/pci/devices") {
        Ok(e) => e,
        Err(_) => return out,
    };

    for entry in entries.flatten() {
        let dev_path = entry.path();

        // class file e.g. "0x030000"
        let class = match read_sysfs(&dev_path.join("class")) {
            Some(c) => c.trim().to_ascii_lowercase(),
            None => continue,
        };

        // 0x030000 = VGA, 0x030200 = 3D controller, 0x030080 = other display
        let is_display = class.starts_with("0x030");
        if !is_display {
            continue;
        }

        let power_control = dev_path.join("power").join("control");

        // amdgpu exposes DPM control under the drm card device, which is a
        // symlink target of this PCI device. Check the PCI device path first
        // (some kernels expose it there), then try drm card links.
        let dpm = dev_path
            .join("power_dpm_force_performance_level")
            .exists()
            .then(|| dev_path.join("power_dpm_force_performance_level"))
            .or_else(|| find_amdgpu_dpm_via_drm(&dev_path));

        out.push(GpuDevice {
            power_control,
            power_dpm_force_performance_level: dpm,
        });
    }

    out
}

/// Try to locate `power_dpm_force_performance_level` via the drm card symlinks
/// that point back to this PCI device.
fn find_amdgpu_dpm_via_drm(pci_dev: &Path) -> Option<PathBuf> {
    let pci_name = pci_dev.file_name()?.to_str()?;
    let drm = Path::new("/sys/class/drm");

    let entries = fs::read_dir(drm).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("card") || name.contains('-') {
            // Skip render nodes (cardN-render-*) etc.; we want the primary card device.
            continue;
        }

        let device_link = entry.path().join("device");
        let resolved = fs::canonicalize(&device_link).ok();
        if let Some(resolved) = resolved {
            let resolved_name = resolved.file_name().and_then(|n| n.to_str());
            if resolved_name == Some(pci_name) {
                let dpm = entry
                    .path()
                    .join("device")
                    .join("power_dpm_force_performance_level");
                if dpm.exists() {
                    return Some(dpm);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_starts_inactive_and_clean() {
        let mut c = LowPowerController::new();
        assert!(!c.is_active());
        assert_eq!(c.exit(), 0);
    }

    #[test]
    fn enter_then_exit_cycles_cleanly() {
        let mut c = LowPowerController::new();
        // On a machine without writable sysfs (test env), enter() is a safe no-op.
        let _ = c.enter();
        assert!(c.is_active());
        let _ = c.exit();
        assert!(!c.is_active());
    }
}
