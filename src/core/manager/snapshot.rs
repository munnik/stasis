// Author: Dustin Pilgrim
// License: GPL-3.0-only

use crate::core::{
    info::{InfoSnapshot, WatchEvent, WaybarInfo},
    state::State,
};

use super::Manager;

impl Manager {
    pub fn snapshot(&self, state: &State, now_ms: u64) -> InfoSnapshot {
        let cfg_opt = self
            .cfg_file
            .effective_for(state.active_profile(), state.plan_source());

        let (text, alt) = status_labels(state);

        let profile = Some(state.active_profile().unwrap_or("default").to_string());

        let rendered = crate::core::manager::info::render_info(cfg_opt.as_ref(), state, now_ms);

        let waybar = WaybarInfo {
            text: text.to_string(),
            alt: alt.to_string(),
            class: alt.to_string(),
            tooltip: rendered.tooltip,
            profile,
        };

        InfoSnapshot::new(waybar, rendered.pretty, state.manually_paused())
    }

    pub fn watch_event(&self, state: &State) -> WatchEvent {
        let (state_name, _) = status_labels(state);

        WatchEvent {
            state: state_name.to_string(),
            paused: state.paused(),
            manually_paused: state.manually_paused(),
            profile: state.active_profile().unwrap_or("default").to_string(),
        }
    }
}

fn status_labels(state: &State) -> (&'static str, &'static str) {
    if state.is_locked() {
        ("locked", "locked")
    } else if state.manually_paused() {
        ("manual", "manually_inhibited")
    } else if state.inhibitors_active() || state.system_paused() {
        ("inhibited", "idle_inhibited")
    } else if state.debounce_pending() {
        ("waiting", "idle_waiting")
    } else {
        ("active", "idle_active")
    }
}
