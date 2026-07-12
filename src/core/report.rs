// Author: Dustin Pilgrim
// License: GPL-3.0-only

//! Power-saving telemetry: episode recording + `stasis report` aggregation.
//!
//! The daemon records completed episodes to a JSONL file at
//! `$XDG_STATE_HOME/stasis/report.jsonl` (default `~/.local/state/stasis/`).
//! Each line is one completed episode:
//!
//! ```json
//! {"kind":"low_power","start":1234567890000,"end":1234567950000}
//! ```
//!
//! `stasis report` reads this file directly (no IPC needed) and aggregates
//! durations per kind for the requested time window.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ---------------- data model ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeKind {
    /// Display off (DPMS step fired → activity resumed).
    DisplayOff,
    /// Hardware low-power mode active.
    LowPower,
    /// System suspended (PrepareForSleep → ResumedFromSleep).
    Suspend,
}

/// A completed episode written to disk.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Episode {
    kind: EpisodeKind,
    start: u64, // epoch ms
    end: u64,   // epoch ms
}

/// Conservative per-kind estimated watt savings used for the energy estimate.
/// These are rough desktop/laptop averages; real savings vary by hardware.
fn estimated_watts(kind: EpisodeKind) -> f64 {
    match kind {
        EpisodeKind::DisplayOff => 10.0,
        EpisodeKind::LowPower => 15.0,
        EpisodeKind::Suspend => 30.0,
    }
}

// ---------------- path ----------------

pub fn report_data_path() -> Option<PathBuf> {
    let state_base =
        dirs::state_dir().or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))?;
    Some(state_base.join("stasis").join("report.jsonl"))
}

// ---------------- recorder (daemon side) ----------------

/// Tracks open episodes in memory and appends completed episodes to disk.
pub struct ReportRecorder {
    open: HashMap<EpisodeKind, u64>,
    path: PathBuf,
}

impl ReportRecorder {
    pub fn new() -> Self {
        let path = report_data_path().unwrap_or_else(|| PathBuf::from("report.jsonl"));
        Self {
            open: HashMap::new(),
            path,
        }
    }

    /// Record the start of an episode. If one of this kind is already open,
    /// the previous start is discarded (defensive — should not normally happen).
    pub fn start(&mut self, kind: EpisodeKind, now_ms: u64) {
        self.open.insert(kind, now_ms);
    }

    /// Record the end of an open episode and persist it to disk.
    pub fn end(&mut self, kind: EpisodeKind, now_ms: u64) {
        if let Some(start) = self.open.remove(&kind) {
            let ep = Episode {
                kind,
                start,
                end: now_ms.max(start),
            };
            self.append(&ep);
        }
    }

    /// Flush all open episodes (e.g. on daemon shutdown).
    pub fn flush(&mut self, now_ms: u64) {
        let kinds: Vec<EpisodeKind> = self.open.keys().copied().collect();
        for kind in kinds {
            self.end(kind, now_ms);
        }
    }

    fn append(&self, ep: &Episode) {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let line = serde_json::to_string(ep).unwrap_or_default();

        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "{line}");
            }
            Err(e) => {
                eventline::debug!("report: could not write episode: {e}");
            }
        }
    }
}

impl Default for ReportRecorder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------- aggregation (CLI side) ----------------

#[derive(Debug, Clone, Copy)]
pub enum ReportRange {
    Today,
    Week,
}

#[derive(Debug, Clone, Default)]
pub struct ReportSummary {
    pub display_off_ms: u64,
    pub low_power_ms: u64,
    pub suspend_ms: u64,
    pub episode_count: usize,
}

impl ReportSummary {
    fn add(&mut self, ep: &Episode) {
        self.episode_count += 1;
        match ep.kind {
            EpisodeKind::DisplayOff => self.display_off_ms += ep.end - ep.start,
            EpisodeKind::LowPower => self.low_power_ms += ep.end - ep.start,
            EpisodeKind::Suspend => self.suspend_ms += ep.end - ep.start,
        }
    }

    /// Rough estimated energy saved in kWh, summed across all kinds.
    pub fn estimated_kwh(&self) -> f64 {
        let mut kwh = 0.0f64;
        kwh += estimated_watts(EpisodeKind::DisplayOff) * self.display_off_ms as f64
            / 3_600_000.0
            / 1000.0;
        kwh += estimated_watts(EpisodeKind::LowPower) * self.low_power_ms as f64
            / 3_600_000.0
            / 1000.0;
        kwh +=
            estimated_watts(EpisodeKind::Suspend) * self.suspend_ms as f64 / 3_600_000.0 / 1000.0;
        kwh
    }

    pub fn confidence(&self) -> &'static str {
        if self.episode_count == 0 {
            "none"
        } else {
            // Estimates, not wall-metered measurements.
            "low (estimated)"
        }
    }
}

/// Read all episodes from the data file.
fn read_all_episodes(path: &PathBuf) -> Vec<Episode> {
    let mut out = Vec::new();

    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return out,
    };

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Episode>(line) {
            Ok(ep) => out.push(ep),
            Err(_) => continue,
        }
    }

    out
}

/// Build a report summary for the given range.
pub fn build_report(range: ReportRange) -> Result<ReportSummary, String> {
    let path = report_data_path().ok_or("could not determine state directory")?;

    let episodes = read_all_episodes(&path);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let cutoff_ms = match range {
        ReportRange::Today => midnight_today_ms().unwrap_or(now),
        ReportRange::Week => now.saturating_sub(7 * 24 * 3_600_000),
    };

    let mut summary = ReportSummary::default();
    for ep in &episodes {
        // Include an episode if it ENDED within the window.
        if ep.end >= cutoff_ms {
            // Clip the portion before the cutoff so partial overnight episodes
            // only count the time inside the window.
            let effective_start = ep.start.max(cutoff_ms);
            summary.add(&Episode {
                kind: ep.kind,
                start: effective_start,
                end: ep.end,
            });
        }
    }

    Ok(summary)
}

/// Local-midnight today as epoch ms, using `chrono` Local.
fn midnight_today_ms() -> Option<u64> {
    use chrono::{Local, NaiveTime, TimeZone};

    let now = Local::now();
    let midnight = now.date_naive().and_time(NaiveTime::from_hms_opt(0, 0, 0)?);
    let local_midnight = now.timezone().from_local_datetime(&midnight).single()?;

    Some(local_midnight.timestamp_millis() as u64)
}

// ---------------- formatting ----------------

pub fn format_duration(ms: u64) -> String {
    let total_secs = ms / 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{total_secs}s")
    }
}

/// Render the full text report.
pub fn render_report(range: ReportRange, summary: &ReportSummary) -> String {
    let label = match range {
        ReportRange::Today => "Today",
        ReportRange::Week => "This Week",
    };

    let kwh = summary.estimated_kwh();

    let mut out = String::new();
    out.push_str("Stasis Report\n");
    out.push_str(&format!("\n{label}\n"));
    out.push_str(&format!(
        "  Display off:   {}\n",
        format_duration(summary.display_off_ms)
    ));
    out.push_str(&format!(
        "  Low power:     {}\n",
        format_duration(summary.low_power_ms)
    ));
    out.push_str(&format!(
        "  Suspended:     {}\n",
        format_duration(summary.suspend_ms)
    ));

    if summary.episode_count > 0 {
        out.push_str(&format!("\n  Estimated energy saved: {:.2} kWh\n", kwh));
        out.push_str(&format!(
            "  Estimate confidence:    {}\n",
            summary.confidence()
        ));
    } else {
        out.push_str("\n  (no idle episodes recorded yet)\n");
    }

    out
}

// ---------------- tests ----------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_basic() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45_000), "45s");
        assert_eq!(format_duration(60_000), "1m");
        assert_eq!(format_duration(3_660_000), "1h 1m");
        assert_eq!(format_duration(19_152_000), "5h 19m");
    }

    #[test]
    fn summary_aggregates_kinds() {
        let mut s = ReportSummary::default();
        s.add(&Episode {
            kind: EpisodeKind::LowPower,
            start: 0,
            end: 3_600_000,
        });
        s.add(&Episode {
            kind: EpisodeKind::LowPower,
            start: 0,
            end: 1_800_000,
        });
        s.add(&Episode {
            kind: EpisodeKind::Suspend,
            start: 0,
            end: 7_200_000,
        });
        assert_eq!(s.low_power_ms, 5_400_000); // 1.5h
        assert_eq!(s.suspend_ms, 7_200_000); // 2h
        assert_eq!(s.episode_count, 3);
    }

    #[test]
    fn estimated_kwh_is_positive() {
        let mut s = ReportSummary::default();
        s.add(&Episode {
            kind: EpisodeKind::LowPower,
            start: 0,
            end: 3_600_000, // 1 hour
        });
        let kwh = s.estimated_kwh();
        assert!(kwh > 0.0);
        // 15W * 1h / 1000 = 0.015 kWh
        assert!((kwh - 0.015).abs() < 0.001);
    }

    #[test]
    fn empty_summary_has_zero_durations() {
        let s = ReportSummary::default();
        assert_eq!(s.display_off_ms, 0);
        assert_eq!(s.low_power_ms, 0);
        assert_eq!(s.suspend_ms, 0);
        assert_eq!(s.episode_count, 0);
        assert_eq!(s.confidence(), "none");
    }

    #[test]
    fn render_report_with_data() {
        let mut s = ReportSummary::default();
        s.add(&Episode {
            kind: EpisodeKind::LowPower,
            start: 0,
            end: 8_100_000, // 2h 15m
        });
        let out = render_report(ReportRange::Today, &s);
        assert!(out.contains("Low power:     2h 15m"));
        assert!(out.contains("Estimated energy saved"));
    }
}
