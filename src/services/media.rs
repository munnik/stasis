// Author: Dustin Pilgrim
// License: GPL-3.0-only

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::core::config::Pattern;
use crate::core::events::{Event, MediaState};
use crate::core::manager_msg::ManagerMsg;

#[derive(Debug, Clone)]
pub struct MediaRules {
    pub epoch: u64, // forces watch::changed() on profile/reload even if values are identical
    pub monitor_media: bool,
    pub ignore_remote_media: bool,
    pub media_blacklist: Vec<Pattern>,
}

/// Spawnable task: polls PulseAudio/PipeWire sink-input state for non-browser,
/// non-game media and emits events on change.
pub async fn run_media(tx: mpsc::Sender<ManagerMsg>, mut rules_rx: watch::Receiver<MediaRules>) {
    let initial = rules_rx.borrow().clone();
    let mut last_epoch = initial.epoch;
    let mut ignore_first_epoch_bump = true;

    let mut svc = MediaService::new(initial.ignore_remote_media, initial.media_blacklist.clone())
        .with_poll_interval_ms(500);

    eventline::info!(
        "media: started (monitor_media={}, ignore_remote_media={}, blacklist_len={}, backend={}) [pactl-sink-input-truth]",
        initial.monitor_media,
        initial.ignore_remote_media,
        svc.blacklist_len(),
        svc.backend_name(),
    );

    if initial.monitor_media {
        svc.force_emit_next();
        let now_ms = crate::core::utils::now_ms();
        if let Some(evs) = svc.poll(now_ms) {
            for ev in evs {
                if tx.send(ManagerMsg::Event(ev)).await.is_err() {
                    return;
                }
            }
        }
    } else {
        seed_idle(&tx).await;
    }

    let sleep_ms = 250u64;
    let mut last_enabled = initial.monitor_media;

    loop {
        tokio::select! {
            changed = rules_rx.changed() => {
                if changed.is_err() {
                    return;
                }

                let rules = rules_rx.borrow().clone();
                let MediaRules { epoch, monitor_media, ignore_remote_media, media_blacklist } = rules;

                let epoch_bumped = epoch != last_epoch;
                if epoch_bumped {
                    last_epoch = epoch;
                }

                svc.reconfigure(ignore_remote_media, media_blacklist.clone());

                if monitor_media != last_enabled {
                    last_enabled = monitor_media;

                    if monitor_media {
                        svc.force_emit_next();
                        let now_ms = crate::core::utils::now_ms();
                        if let Some(evs) = svc.poll(now_ms) {
                            for ev in evs {
                                if tx.send(ManagerMsg::Event(ev)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    } else {
                        seed_idle(&tx).await;
                    }
                } else if monitor_media && epoch_bumped {
                    if ignore_first_epoch_bump {
                        ignore_first_epoch_bump = false;
                    } else {
                        svc.force_emit_next();
                        let now_ms = crate::core::utils::now_ms();
                        if let Some(evs) = svc.poll(now_ms) {
                            for ev in evs {
                                if tx.send(ManagerMsg::Event(ev)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
            }

            _ = tokio::time::sleep(Duration::from_millis(sleep_ms)) => {
                let rules = rules_rx.borrow().clone();
                if !rules.monitor_media {
                    continue;
                }

                let now_ms = crate::core::utils::now_ms();
                if let Some(evs) = svc.poll(now_ms) {
                    for ev in evs {
                        if tx.send(ManagerMsg::Event(ev)).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

async fn seed_idle(tx: &mpsc::Sender<ManagerMsg>) {
    let now_ms = crate::core::utils::now_ms();
    for ev in [
        Event::MediaInhibitorCount { count: 0, now_ms },
        Event::MediaStateChanged {
            state: MediaState::Idle,
            now_ms,
        },
    ] {
        let _ = tx.send(ManagerMsg::Event(ev)).await;
    }
}

#[derive(Debug)]
pub struct MediaService {
    ignore_remote_media: bool,
    media_blacklist: Vec<Pattern>,
    backend: MediaBackend,

    poll_interval_ms: u64,
    last_poll_ms: u64,

    last_count: Option<u64>,
    last_state: Option<MediaState>,

    force_emit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaBackend {
    Pactl,
    None,
}

impl MediaService {
    pub fn new(ignore_remote_media: bool, media_blacklist: Vec<Pattern>) -> Self {
        Self {
            ignore_remote_media,
            media_blacklist,
            backend: detect_backend(),
            poll_interval_ms: 1000,
            last_poll_ms: 0,
            last_count: None,
            last_state: None,
            force_emit: false,
        }
    }

    pub fn with_poll_interval_ms(mut self, ms: u64) -> Self {
        self.poll_interval_ms = ms.max(100);
        self
    }

    pub fn blacklist_len(&self) -> usize {
        self.media_blacklist.len()
    }

    pub fn backend_name(&self) -> &'static str {
        match self.backend {
            MediaBackend::Pactl => "pactl",
            MediaBackend::None => "none",
        }
    }

    pub fn reconfigure(&mut self, ignore_remote_media: bool, media_blacklist: Vec<Pattern>) {
        let changed = self.ignore_remote_media != ignore_remote_media
            || !patterns_same(&self.media_blacklist, &media_blacklist);

        self.ignore_remote_media = ignore_remote_media;
        self.media_blacklist = media_blacklist;

        if changed {
            self.force_emit_next();
            eventline::info!(
                "media: reconfigured (ignore_remote_media={}, blacklist_len={})",
                self.ignore_remote_media,
                self.media_blacklist.len()
            );
        }
    }

    pub fn force_emit_next(&mut self) {
        self.force_emit = true;
        self.last_poll_ms = 0;
    }

    pub fn poll(&mut self, now_ms: u64) -> Option<Vec<Event>> {
        if now_ms < self.last_poll_ms.saturating_add(self.poll_interval_ms) {
            return None;
        }
        self.last_poll_ms = now_ms;

        let count = match self.backend {
            MediaBackend::Pactl => {
                match pactl_sink_input_count(self.ignore_remote_media, &self.media_blacklist) {
                    Ok(n) => n,
                    Err(e) => {
                        eventline::warn!(
                            "media: pactl sink-input query failed (keeping previous): {}",
                            e
                        );
                        self.last_count.unwrap_or(0)
                    }
                }
            }
            MediaBackend::None => 0,
        };

        let state = if count > 0 {
            MediaState::PlayingLocal
        } else {
            MediaState::Idle
        };

        self.emit_state(now_ms, count, state)
    }

    fn emit_state(&mut self, now_ms: u64, count: u64, state: MediaState) -> Option<Vec<Event>> {
        let first_poll = self.last_count.is_none() && self.last_state.is_none();
        let prev_count = self.last_count.unwrap_or(0);

        let count_changed = !first_poll && self.last_count != Some(count);
        let state_changed = !first_poll && self.last_state != Some(state);

        if count_changed {
            eventline::info!(
                "media: count {} -> {} (state={:?})",
                prev_count,
                count,
                state
            );
        } else if (first_poll || self.force_emit) && count != 0 {
            eventline::info!("media: count {} -> {} (state={:?})", 0u64, count, state);
        }

        if first_poll || count_changed || state_changed || self.force_emit {
            self.last_count = Some(count);
            self.last_state = Some(state);

            let out = vec![
                Event::MediaInhibitorCount { count, now_ms },
                Event::MediaStateChanged { state, now_ms },
            ];

            self.force_emit = false;
            return Some(out);
        }

        None
    }
}

fn session_env_cmd(program: &str) -> Command {
    let mut cmd = Command::new(program);
    // Forward session env vars so pactl can reach PipeWire/PulseAudio when
    // stasis is running as a systemd user service or otherwise outside the
    // user session environment.
    for var in &[
        "PULSE_SERVER",
        "PULSE_RUNTIME_PATH",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "WAYLAND_DISPLAY",
        "DISPLAY",
    ] {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
    cmd
}

fn detect_backend() -> MediaBackend {
    if pactl_available() {
        return MediaBackend::Pactl;
    }

    MediaBackend::None
}

fn pactl_available() -> bool {
    session_env_cmd("pactl")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn pactl_sink_input_count(
    ignore_remote_media: bool,
    media_blacklist: &[Pattern],
) -> Result<u64, String> {
    let out = session_env_cmd("pactl")
        .args(["list", "sink-inputs"])
        .output()
        .map_err(|e| format!("pactl spawn failed: {e}"))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("pactl list sink-inputs failed: {}", err.trim()));
    }

    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        return Ok(0);
    }

    let mut count = 0u64;
    let mut block = String::new();
    let mut saw_header = false;

    for line in text.lines() {
        let trimmed = line.trim_start();
        let is_header = trimmed.starts_with("Sink Input #") || trimmed.starts_with("SinkInput #");

        if is_header {
            if saw_header && sink_input_block_counts(&block, ignore_remote_media, media_blacklist) {
                count += 1;
            }
            block.clear();
            saw_header = true;
        }

        if saw_header {
            block.push_str(line);
            block.push('\n');
        }
    }

    if saw_header && sink_input_block_counts(&block, ignore_remote_media, media_blacklist) {
        count += 1;
    }

    Ok(count)
}

fn sink_input_block_counts(
    block: &str,
    ignore_remote_media: bool,
    media_blacklist: &[Pattern],
) -> bool {
    let props = parse_pactl_properties(block);

    if props.is_empty() {
        return false;
    }

    if sink_input_is_corked(block) {
        return false;
    }

    if sink_input_is_muted(block) {
        return false;
    }

    // Browser/media-session ownership belongs in dbus.rs. media.rs should only
    // act as a narrow local-audio fallback, so aggressively exclude browser,
    // TTS/synthetic, and other system-ish streams here.
    if sink_input_is_browser(&props) {
        return false;
    }

    if sink_input_is_synthetic_or_tts(&props) {
        return false;
    }

    if sink_input_is_systemish(&props) {
        return false;
    }

    if sink_input_is_game(&props) {
        return false;
    }

    if sink_input_is_blacklisted(media_blacklist, &props) {
        return false;
    }

    if ignore_remote_media && sink_input_is_remote(&props) {
        return false;
    }

    true
}

fn sink_input_is_corked(block: &str) -> bool {
    block.lines().any(|line| {
        let t = line.trim();
        t.eq_ignore_ascii_case("Corked: yes")
    })
}

fn sink_input_is_muted(block: &str) -> bool {
    block.lines().any(|line| {
        let t = line.trim();
        t.eq_ignore_ascii_case("Mute: yes")
    })
}

fn parse_pactl_properties(block: &str) -> HashMap<String, String> {
    let mut props = HashMap::new();
    let mut in_properties = false;

    for line in block.lines() {
        let trimmed = line.trim();

        if trimmed == "Properties:" {
            in_properties = true;
            continue;
        }

        if !in_properties {
            continue;
        }

        if trimmed.is_empty() {
            continue;
        }

        if !line.starts_with('\t') && !line.starts_with(' ') {
            break;
        }

        let Some((k, v)) = trimmed.split_once('=') else {
            continue;
        };

        let key = k.trim().to_ascii_lowercase();
        let value = v.trim().trim_matches('"').to_string();

        props.insert(key, value);
    }

    props
}

fn sink_input_is_synthetic_or_tts(props: &HashMap<String, String>) -> bool {
    const NEEDLES: &[&str] = &[
        "speech-dispatcher",
        "speech dispatcher",
        "speech-dispatcher-dummy",
        "sd_dummy",
        "speechd",
        "espeak",
        "espeak-ng",
        "festival",
        "flite",
        "piper",
        "rhvoice",
        "orca",
        "screen reader",
        "accessibility",
    ];

    haystack_contains_any(&sink_input_identity_haystack(props), NEEDLES)
}

fn sink_input_is_systemish(props: &HashMap<String, String>) -> bool {
    const NEEDLES: &[&str] = &[
        "event sound",
        "notification",
        "system sound",
        "alert",
        "bell",
        "beep",
        "xdg-desktop-portal",
        "wireplumber",
        // "pipewire" intentionally omitted — matches "pipewire-pulse" in
        // client.api on every PipeWire sink input; wireplumber covers the
        // internal streams we actually want to exclude
    ];

    haystack_contains_any(&sink_input_identity_haystack(props), NEEDLES)
}

fn sink_input_is_blacklisted(blacklist: &[Pattern], props: &HashMap<String, String>) -> bool {
    if blacklist.is_empty() {
        return false;
    }

    let hay_lc = sink_input_haystack(props);

    blacklist.iter().any(|p| p.matches_lc(&hay_lc))
}

fn sink_input_is_browser(props: &HashMap<String, String>) -> bool {
    const NEEDLES: &[&str] = &[
        "firefox",
        "chromium",
        "google-chrome",
        "google chrome",
        "chrome",
        "brave",
        "vivaldi",
        "microsoft-edge",
        "msedge",
        "opera",
        "tor browser",
        "zen browser",
        "zen-browser",
        "waterfox",
        "librewolf",
    ];

    haystack_contains_any(&sink_input_identity_haystack(props), NEEDLES)
}

fn sink_input_is_game(props: &HashMap<String, String>) -> bool {
    const NEEDLES: &[&str] = &[
        "steam",
        "gamescope",
        "lutris",
        "heroic",
        "prismlauncher",
        "minecraft",
        "wine",
        "proton",
        "retroarch",
        "dolphin-emu",
        "pcsx2",
        "rpcs3",
        "citra",
        "yuzu",
        "ryujinx",
    ];

    haystack_contains_any(&sink_input_identity_haystack(props), NEEDLES)
}

fn sink_input_is_remote(props: &HashMap<String, String>) -> bool {
    const NEEDLES: &[&str] = &[
        "spotify connect",
        "chromecast",
        "airplay",
        "raop",
        "dlna",
        "upnp",
        "snapcast",
        "shairport",
        "network stream",
        "http://",
        "https://",
        "rtsp://",
        "rtmp://",
        "mms://",
        "icy://",
    ];

    haystack_contains_any(&sink_input_identity_haystack(props), NEEDLES)
}

/// Haystack restricted to identity fields only (process binary, app name, etc).
/// Use this for browser/game/systemish/tts/remote filters so that technical
/// plumbing fields like `client.api = "pipewire-pulse"` can never cause false
/// positives.
fn sink_input_identity_haystack(props: &HashMap<String, String>) -> String {
    let identity_keys = [
        "application.name",
        "application.process.binary",
        "application.process.executable",
        "application.icon_name",
        "app.name",
        "application.id",
        "node.name",
        "node.description",
    ];

    let mut parts = Vec::new();
    for key in identity_keys {
        if let Some(v) = props.get(key) {
            if !v.trim().is_empty() {
                parts.push(v.trim().to_string());
            }
        }
    }

    parts.join(" ").to_lowercase()
}

fn sink_input_haystack(props: &HashMap<String, String>) -> String {
    let ordered_keys = [
        "application.name",
        "application.process.binary",
        "application.process.executable",
        "application.icon_name",
        "media.name",
        "media.title",
        "media.artist",
        "media.filename",
        "media.role",
        "node.name",
        "node.description",
        "application.id",
        "app.name",
    ];

    let mut parts = Vec::new();

    for key in ordered_keys {
        if let Some(v) = props.get(key) {
            if !v.trim().is_empty() {
                parts.push(v.trim().to_string());
            }
        }
    }

    for (k, v) in props {
        if ordered_keys.contains(&k.as_str()) {
            continue;
        }
        if !v.trim().is_empty() {
            parts.push(format!("{k} {}", v.trim()));
        }
    }

    parts.join(" ").to_lowercase()
}

fn haystack_contains_any(hay_lc: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay_lc.contains(n))
}

fn pattern_key(p: &Pattern) -> String {
    match p {
        Pattern::Literal(s) => s.clone(),
        Pattern::Regex(r) => format!("/{}/", r.as_str()),
    }
}

fn patterns_same(a: &[Pattern], b: &[Pattern]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    a.iter()
        .map(pattern_key)
        .zip(b.iter().map(pattern_key))
        .all(|(x, y)| x == y)
}
