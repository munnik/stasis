// Author: Dustin Pilgrim
// License: GPL-3.0-only

use std::collections::HashSet;
use std::env;
use std::mem;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::core::config::Pattern;
use crate::core::events::Event;
use crate::core::manager_msg::ManagerMsg;

#[derive(Debug, Clone)]
pub struct AppRules {
    pub epoch: u64,
    pub apps: Vec<Pattern>,
}

/// Spawnable task: periodically polls app inhibitors and emits events on change.
///
/// Logging policy (INFO):
/// - If count changes: log "X -> Y"
/// - If we are forcing a refresh (profile/reload/rules changed) OR first poll:
///     log "0 -> N" ONLY when N != 0
pub async fn run_app_inhibit(
    tx: mpsc::Sender<ManagerMsg>,
    mut rules_rx: watch::Receiver<AppRules>,
) {
    let initial = rules_rx.borrow().clone();
    let mut last_epoch = initial.epoch;

    let mut svc = AppInhibitService::new(&initial.apps).with_poll_interval_ms(1000);

    eventline::info!("app_inhibit: started (backend={})", svc.backend_name());

    // Ensure we emit once immediately (and log 0->N if N!=0).
    svc.force_emit_next();

    let sleep_ms = 250u64;

    loop {
        tokio::select! {
            changed = rules_rx.changed() => {
                if changed.is_err() {
                    return;
                }

                let rules = rules_rx.borrow().clone();

                let epoch_bumped = rules.epoch != last_epoch;
                if epoch_bumped {
                    last_epoch = rules.epoch;
                    svc.force_emit_next();
                }

                svc.reconfigure(&rules.apps);

                let now_ms = crate::core::utils::now_ms();
                if let Some(ev) = svc.poll_async(now_ms).await {
                    if tx.send(ManagerMsg::Event(ev)).await.is_err() {
                        return;
                    }
                }
            }

            _ = tokio::time::sleep(Duration::from_millis(sleep_ms)) => {
                let now_ms = crate::core::utils::now_ms();
                if let Some(ev) = svc.poll_async(now_ms).await {
                    if tx.send(ManagerMsg::Event(ev)).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct AppInhibitService {
    apps: Vec<Pattern>,
    backend: Backend,

    poll_interval_ms: u64,
    last_poll_ms: u64,

    last_count: Option<u64>, // None => never polled yet
    force_emit: bool,        // next poll must emit and do baseline logging

    /// Reused scratch buffer — `clear()`ed before every poll, never dropped.
    /// We `mem::take` it into `spawn_blocking` and restore it on return so we
    /// never allocate a new HashSet on the hot path. If the task panics, the
    /// field is left as an empty default and a fresh allocation occurs next poll.
    seen: HashSet<String>,
}

#[derive(Debug)]
enum Backend {
    Halley(HalleyBackend),
    Hyprland(HyprlandBackend),
    Niri(NiriBackend),
    Proc(ProcBackend),
}

#[derive(Debug, Default)]
struct HalleyBackend {}

#[derive(Debug, Default)]
struct HyprlandBackend {}

#[derive(Debug, Default)]
struct NiriBackend {}

#[derive(Debug, Default)]
struct ProcBackend {}

impl AppInhibitService {
    pub fn new(inhibit_apps: &[Pattern]) -> Self {
        let apps = normalize_patterns(inhibit_apps);
        let backend = detect_backend().unwrap_or_else(|| Backend::Proc(ProcBackend::default()));

        Self {
            apps,
            backend,
            poll_interval_ms: 1000,
            last_poll_ms: 0,
            last_count: None,
            force_emit: false,
            // Start small; the shrink logic below keeps it tight at steady state.
            seen: HashSet::with_capacity(8),
        }
    }

    pub fn with_poll_interval_ms(mut self, ms: u64) -> Self {
        self.poll_interval_ms = ms.max(100);
        self
    }

    pub fn reconfigure(&mut self, inhibit_apps: &[Pattern]) {
        let new_apps = normalize_patterns(inhibit_apps);

        if patterns_same(&self.apps, &new_apps) {
            return;
        }

        self.apps = new_apps;
        self.force_emit_next();

        eventline::info!(
            "app_inhibit: reconfigured (apps_len={}, backend={})",
            self.apps.len(),
            self.backend_name(),
        );
    }

    pub fn force_emit_next(&mut self) {
        self.force_emit = true;
        self.last_poll_ms = 0;
    }

    /// Async-aware poll. Subprocess/fs queries run via `spawn_blocking` so the
    /// tokio executor thread is never blocked.
    ///
    /// The `seen` scratch buffer is moved into the blocking task via `mem::take`
    /// and returned alongside the count so its backing allocation survives across
    /// polls — no HashSet is allocated on the steady-state hot path.
    pub async fn poll_async(&mut self, now_ms: u64) -> Option<Event> {
        if now_ms < self.last_poll_ms.saturating_add(self.poll_interval_ms) {
            return None;
        }
        self.last_poll_ms = now_ms;

        let prev_count = self.last_count.unwrap_or(0);

        let count = if self.apps.is_empty() {
            // Nothing to match — rinse the buffer so stale entries can't linger.
            self.seen.clear();
            0
        } else {
            // Take the scratch buffer out so we can move it into spawn_blocking.
            // self.seen is left as HashSet::default() (empty, no allocation).
            let mut scratch = mem::take(&mut self.seen);
            scratch.clear();

            match &self.backend {
                Backend::Halley(_) => {
                    let apps = self.apps.clone();
                    match tokio::task::spawn_blocking(move || {
                        HalleyBackend::count_into(&apps, &mut scratch)?;
                        Ok::<_, String>((scratch.len() as u64, scratch))
                    })
                    .await
                    {
                        Ok(Ok((n, returned))) => {
                            self.seen = returned;
                            n
                        }
                        Ok(Err(e)) => {
                            eventline::warn!(
                                "app_inhibit: halley query failed (keeping previous count={}): {}",
                                prev_count,
                                e
                            );
                            prev_count
                        }
                        Err(e) => {
                            eventline::warn!(
                                "app_inhibit: halley task panicked (keeping previous count={}): {}",
                                prev_count,
                                e
                            );
                            prev_count
                        }
                    }
                }

                Backend::Hyprland(_) => {
                    let apps = self.apps.clone();
                    match tokio::task::spawn_blocking(move || {
                        HyprlandBackend::count_into(&apps, &mut scratch)?;
                        Ok::<_, String>((scratch.len() as u64, scratch))
                    })
                    .await
                    {
                        Ok(Ok((n, returned))) => {
                            self.seen = returned;
                            n
                        }
                        Ok(Err(e)) => {
                            eventline::warn!(
                                "app_inhibit: hyprland query failed (keeping previous count={}): {}",
                                prev_count,
                                e
                            );
                            prev_count
                        }
                        Err(e) => {
                            eventline::warn!(
                                "app_inhibit: hyprland task panicked (keeping previous count={}): {}",
                                prev_count,
                                e
                            );
                            prev_count
                        }
                    }
                }

                Backend::Niri(_) => {
                    let apps = self.apps.clone();
                    match tokio::task::spawn_blocking(move || {
                        NiriBackend::count_into(&apps, &mut scratch)?;
                        Ok::<_, String>((scratch.len() as u64, scratch))
                    })
                    .await
                    {
                        Ok(Ok((n, returned))) => {
                            self.seen = returned;
                            n
                        }
                        Ok(Err(e)) => {
                            eventline::warn!(
                                "app_inhibit: niri query failed (keeping previous count={}): {}",
                                prev_count,
                                e
                            );
                            prev_count
                        }
                        Err(e) => {
                            eventline::warn!(
                                "app_inhibit: niri task panicked (keeping previous count={}): {}",
                                prev_count,
                                e
                            );
                            prev_count
                        }
                    }
                }

                Backend::Proc(_) => {
                    let apps = self.apps.clone();
                    match tokio::task::spawn_blocking(move || {
                        ProcBackend::count_into(&apps, &mut scratch);
                        (scratch.len() as u64, scratch)
                    })
                    .await
                    {
                        Ok((n, returned)) => {
                            self.seen = returned;
                            n
                        }
                        Err(e) => {
                            eventline::warn!(
                                "app_inhibit: proc task panicked (keeping previous count={}): {}",
                                prev_count,
                                e
                            );
                            prev_count
                        }
                    }
                }
            }
        };

        // Aggressively shrink the scratch buffer if it ballooned beyond what we
        // realistically need. An idle manager typically matches 0–5 apps at once;
        // keeping 32+ empty slots alive wastes RSS indefinitely.
        if self.seen.capacity() > 32 && self.seen.len() < 8 {
            self.seen.shrink_to(8);
        }

        let first_poll = self.last_count.is_none();
        let prev = self.last_count.unwrap_or(0);
        let changed = !first_poll && prev != count;

        if changed {
            eventline::info!(
                "app_inhibit: count {} -> {} (backend={}, apps_len={})",
                prev,
                count,
                self.backend_name(),
                self.apps.len()
            );
        } else if (first_poll || self.force_emit) && count != 0 {
            eventline::info!(
                "app_inhibit: count {} -> {} (backend={}, apps_len={})",
                0u64,
                count,
                self.backend_name(),
                self.apps.len()
            );
        }

        if first_poll || changed || self.force_emit {
            self.last_count = Some(count);
            self.force_emit = false;
            return Some(Event::AppInhibitorCount { count, now_ms });
        }

        None
    }

    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            Backend::Halley(_) => "halley",
            Backend::Hyprland(_) => "hyprland",
            Backend::Niri(_) => "niri",
            Backend::Proc(_) => "proc",
        }
    }
}

// ----------------------------- backend detection -----------------------------

fn detect_backend() -> Option<Backend> {
    detect_halley_backend()
        .or_else(detect_hyprland_backend)
        .or_else(detect_niri_backend)
}

fn detect_halley_backend() -> Option<Backend> {
    for key in [
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "DESKTOP_SESSION",
    ] {
        if let Ok(value) = env::var(key) {
            if value.to_lowercase().contains("halley") {
                return Some(Backend::Halley(HalleyBackend::default()));
            }
        }
    }

    if env::var("HALLEY_WL_BACKEND").is_ok() {
        return Some(Backend::Halley(HalleyBackend::default()));
    }

    None
}

fn detect_hyprland_backend() -> Option<Backend> {
    if env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
        return Some(Backend::Hyprland(HyprlandBackend::default()));
    }

    if let Ok(desktop) = env::var("XDG_CURRENT_DESKTOP") {
        if desktop.to_lowercase().contains("hyprland") {
            return Some(Backend::Hyprland(HyprlandBackend::default()));
        }
    }

    None
}

fn detect_niri_backend() -> Option<Backend> {
    if let Ok(desktop) = env::var("XDG_CURRENT_DESKTOP") {
        if desktop.to_lowercase().contains("niri") {
            return Some(Backend::Niri(NiriBackend::default()));
        }
    }

    if env::var("NIRI_SOCKET").is_ok() {
        return Some(Backend::Niri(NiriBackend::default()));
    }

    None
}

// ----------------------------- matching helpers ------------------------------

/// Returns `true` on the first matching pattern, short-circuiting the rest.
fn should_inhibit_app_id(app_id: &str, patterns: &[Pattern]) -> bool {
    if app_id.is_empty() {
        return false;
    }

    for pat in patterns {
        let matched = match pat {
            Pattern::Literal(s) => app_id_matches_literal(s, app_id),
            Pattern::Regex(r) => r.is_match(app_id),
        };
        if matched {
            return true;
        }
    }

    false
}

fn app_id_matches_literal(pattern: &str, app_id: &str) -> bool {
    // Exact case-insensitive match.
    if pattern.eq_ignore_ascii_case(app_id) {
        return true;
    }
    // "firefox.exe" vs pattern "firefox".
    if app_id.ends_with(".exe") {
        let name = app_id.strip_suffix(".exe").unwrap_or(app_id);
        if pattern.eq_ignore_ascii_case(name) {
            return true;
        }
    }
    // Reverse dotted suffix: pattern "org.mozilla.firefox" vs app_id "firefox".
    if let Some(last) = pattern.split('.').last() {
        if last.eq_ignore_ascii_case(app_id) {
            return true;
        }
    }
    false
}

// ----------------------------- Halley (halleyctl) ----------------------------

impl HalleyBackend {
    /// Populates `seen` with matched Halley window app IDs. Caller must `clear()` first.
    fn count_into(apps: &[Pattern], seen: &mut HashSet<String>) -> Result<(), String> {
        let out = std::process::Command::new("halleyctl")
            .args(["node", "list", "--json"])
            .output()
            .map_err(|e| format!("halleyctl spawn failed: {e}"))?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("halleyctl node list --json failed: {}", err.trim()));
        }

        Self::count_json_into(apps, seen, &out.stdout)
    }

    fn count_json_into(
        apps: &[Pattern],
        seen: &mut HashSet<String>,
        json: &[u8],
    ) -> Result<(), String> {
        let v: serde_json::Value = serde_json::from_slice(json)
            .map_err(|e| format!("halleyctl json parse failed: {e}"))?;

        let outputs = v
            .get("outputs")
            .and_then(|x| x.as_array())
            .ok_or_else(|| "halleyctl json: expected outputs array".to_string())?;

        for output in outputs {
            let Some(nodes) = output.get("nodes").and_then(|x| x.as_array()) else {
                continue;
            };

            for node in nodes {
                let app_id = node.get("app_id").and_then(|x| x.as_str()).unwrap_or("");
                if app_id.is_empty() {
                    continue;
                }

                if should_inhibit_app_id(app_id, apps) {
                    seen.insert(app_id.to_string());
                }
            }
        }

        Ok(())
    }
}

// ----------------------------- Hyprland (hyprctl) ----------------------------

impl HyprlandBackend {
    /// Populates `seen` with matched window classes. Caller must `clear()` first.
    fn count_into(apps: &[Pattern], seen: &mut HashSet<String>) -> Result<(), String> {
        let out = std::process::Command::new("hyprctl")
            .args(["clients", "-j"])
            .output()
            .map_err(|e| format!("hyprctl spawn failed: {e}"))?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("hyprctl clients -j failed: {}", err.trim()));
        }

        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| format!("hyprctl json parse failed: {e}"))?;

        let arr = v
            .as_array()
            .ok_or_else(|| "hyprctl json: expected array".to_string())?;

        for item in arr {
            let class = item.get("class").and_then(|x| x.as_str()).unwrap_or("");
            if class.is_empty() {
                continue;
            }
            if should_inhibit_app_id(class, apps) {
                seen.insert(class.to_string());
            }
        }

        Ok(())
    }
}

// ----------------------------- Niri (niri msg windows) -----------------------

impl NiriBackend {
    /// Populates `seen` with matched app IDs. Caller must `clear()` first.
    fn count_into(apps: &[Pattern], seen: &mut HashSet<String>) -> Result<(), String> {
        let out = std::process::Command::new("niri")
            .args(["msg", "windows"])
            .output()
            .map_err(|e| format!("niri spawn failed: {e}"))?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("niri msg windows failed: {}", err.trim()));
        }

        let text = String::from_utf8_lossy(&out.stdout);

        for line in text.lines() {
            let Some(rest) = line.strip_prefix("  App ID: ") else {
                continue;
            };

            let app_id = rest.trim().trim_matches('"');
            if app_id.is_empty() {
                continue;
            }

            if should_inhibit_app_id(app_id, apps) {
                seen.insert(app_id.to_string());
            }
        }

        Ok(())
    }
}

// ----------------------------- /proc (procfs crate) --------------------------

impl ProcBackend {
    /// Populates `seen` with matched process names via the `procfs` crate.
    ///
    /// The `procfs` crate reads `/proc` entries through a single `read_dir` pass
    /// and parses only the fields we actually need (`stat.comm` and `exe`), which
    /// avoids the overhead of manually re-implementing that logic and the full
    /// process list copy that `sysinfo` kept alive.
    ///
    /// Early-exit: once every literal pattern has a hit in `seen` *and* there
    /// are no regex patterns left to satisfy, further scanning cannot change the
    /// count so we break out of the loop immediately.
    fn count_into(apps: &[Pattern], seen: &mut HashSet<String>) {
        let has_regex = apps.iter().any(|p| matches!(p, Pattern::Regex(_)));

        // Pre-compute the exact keys that literal patterns produce so we can
        // check saturation in O(n_literals) rather than O(n_seen).
        let literal_keys: Vec<String> = apps
            .iter()
            .filter_map(|p| {
                if let Pattern::Literal(s) = p {
                    Some(s.to_lowercase())
                } else {
                    None
                }
            })
            .collect();

        let all_processes = match procfs::process::all_processes() {
            Ok(iter) => iter,
            Err(e) => {
                eventline::warn!("app_inhibit: procfs::all_processes failed: {e}");
                return;
            }
        };

        for prc in all_processes.flatten() {
            // Early-exit when all literal patterns are satisfied and there are no
            // regex patterns that could add new unique keys.
            if !has_regex && literal_keys.iter().all(|k| seen.contains(k.as_str())) {
                break;
            }

            // Primary: `comm` — kernel-truncated to 15 chars but fast and
            // sufficient for most app names.
            let comm_matched = prc
                .stat()
                .ok()
                .and_then(|s| Self::match_key(&s.comm, apps))
                .map(|key| seen.insert(key))
                .is_some();

            if comm_matched {
                continue;
            }

            // Fallback: exe basename — handles wrappers that rename comm or
            // apps whose name is longer than 15 characters.
            if let Ok(exe) = prc.exe() {
                if let Some(name) = exe.file_name().and_then(|n| n.to_str()) {
                    if let Some(key) = Self::match_key(name, apps) {
                        seen.insert(key);
                    }
                }
            }
        }
    }

    #[inline]
    fn match_key(hay: &str, apps: &[Pattern]) -> Option<String> {
        for p in apps {
            let matched = match p {
                Pattern::Literal(s) => hay.eq_ignore_ascii_case(s),
                Pattern::Regex(r) => r.is_match(hay),
            };
            if matched {
                return Some(hay.to_lowercase());
            }
        }
        None
    }
}

// ----------------------------- utils -----------------------------------------

/// Deduplicate patterns by their string representation so duplicate rules from
/// a config reload don't silently inflate match counts.
fn normalize_patterns(inhibit_apps: &[Pattern]) -> Vec<Pattern> {
    let mut seen_strs: HashSet<String> = HashSet::with_capacity(inhibit_apps.len());
    inhibit_apps
        .iter()
        .filter(|p| seen_strs.insert(p.to_string()))
        .cloned()
        .collect()
}

fn patterns_same(a: &[Pattern], b: &[Pattern]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .map(|p| p.to_string())
        .zip(b.iter().map(|p| p.to_string()))
        .all(|(x, y)| x == y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halley_json_counts_unique_matching_app_ids() {
        let apps = vec![
            Pattern::Literal("firefox".to_string()),
            Pattern::Regex(regex::Regex::new(r"steam_app_.*").unwrap()),
        ];
        let json = br#"
        {
          "outputs": [
            {
              "output": "DP-1",
              "nodes": [
                { "id": 1, "app_id": "firefox", "title": "one" },
                { "id": 2, "app_id": "firefox", "title": "two" },
                { "id": 3, "app_id": "kitty", "title": "shell" }
              ]
            },
            {
              "output": "DP-2",
              "nodes": [
                { "id": 4, "app_id": "steam_app_123", "title": "game" },
                { "id": 5, "app_id": null, "title": "missing app" }
              ]
            }
          ]
        }
        "#;

        let mut seen = HashSet::new();
        HalleyBackend::count_json_into(&apps, &mut seen, json).unwrap();

        assert_eq!(seen.len(), 2);
        assert!(seen.contains("firefox"));
        assert!(seen.contains("steam_app_123"));
    }

    #[test]
    fn halley_json_rejects_missing_outputs_array() {
        let mut seen = HashSet::new();
        let err = HalleyBackend::count_json_into(&[], &mut seen, br#"{"nodes": []}"#)
            .expect_err("missing outputs should be invalid");

        assert!(err.contains("expected outputs array"));
    }
}
