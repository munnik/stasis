// Author: Dustin Pilgrim
// License: GPL-3.0-only

use crate::core::action::Action;
use crate::core::config::{Config, ConfigFile, PlanSource, PlanStep, PlanStepKind};
use crate::core::events::{ActivityKind, Event};
use crate::core::manager::Manager;
use crate::core::state::State;

fn cfg_with_plan(plan: Vec<PlanStep>) -> ConfigFile {
    let mut cfg = Config::disabled();

    // effective_for() selects plan_* into cfg.plan; tests must populate plan_desktop.
    cfg.plan_desktop = plan;

    ConfigFile {
        default: cfg,
        profiles: vec![],
        active_profile: None,
    }
}

fn cfg_with_plan_and_notify(
    plan: Vec<PlanStep>,
    debounce_seconds: u64,
    notify_before_action: bool,
) -> ConfigFile {
    let mut cfg = Config::disabled();

    cfg.plan_desktop = plan;
    cfg.debounce_seconds = debounce_seconds;
    cfg.notify_before_action = notify_before_action;

    ConfigFile {
        default: cfg,
        profiles: vec![],
        active_profile: None,
    }
}

fn step(kind: PlanStepKind, timeout: u64, cmd: &str) -> PlanStep {
    PlanStep {
        kind,
        timeout_seconds: timeout,
        command: Some(cmd.to_string()),
        resume_command: None,
        notification: None,
        notify_seconds_before: None,
    }
}

fn enter_idle(mgr: &mut Manager, state: &mut State, now_ms: u64) {
    let _ = mgr
        .handle_event(state, Event::CompositorIdled { now_ms })
        .unwrap();
}

#[test]
fn per_step_timers_chain_from_previous_fire() {
    let plan = vec![
        step(PlanStepKind::Startup, 5, "a"),
        step(PlanStepKind::Dpms, 7, "b"),
    ];

    let mut mgr = Manager::new(cfg_with_plan(plan));
    let mut state = State::new(0);
    state.set_plan_source(PlanSource::Desktop);
    enter_idle(&mut mgr, &mut state, 0);

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 4000 })
        .unwrap();
    assert!(actions.is_empty());

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 5000 })
        .unwrap();
    assert_eq!(actions.len(), 1);

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 11999 })
        .unwrap();
    assert!(actions.is_empty());

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 12000 })
        .unwrap();
    assert_eq!(actions.len(), 1);
}

#[test]
fn skips_disabled_steps() {
    let mut disabled = step(PlanStepKind::Startup, 0, "nope");
    disabled.command = None;

    let plan = vec![disabled, step(PlanStepKind::Dpms, 1, "yes")];

    let mut mgr = Manager::new(cfg_with_plan(plan));
    let mut state = State::new(0);
    state.set_plan_source(PlanSource::Desktop);
    enter_idle(&mut mgr, &mut state, 0);

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 1000 })
        .unwrap();
    assert_eq!(actions.len(), 1);
}

#[test]
fn lock_step_skipped_if_already_locked() {
    let plan = vec![
        step(PlanStepKind::LockScreen, 1, "lock"),
        step(PlanStepKind::Dpms, 1, "dpms"),
    ];

    let mut mgr = Manager::new(cfg_with_plan(plan));
    let mut state = State::new(0);
    state.set_plan_source(PlanSource::Desktop);
    state.set_locked(true);
    enter_idle(&mut mgr, &mut state, 0);

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 1000 })
        .unwrap();

    assert_eq!(actions.len(), 1);
}

#[test]
fn activity_resets_cycle() {
    let plan = vec![
        step(PlanStepKind::Startup, 1, "a"),
        step(PlanStepKind::Dpms, 1, "b"),
    ];

    let mut mgr = Manager::new(cfg_with_plan(plan));
    let mut state = State::new(0);
    state.set_plan_source(PlanSource::Desktop);
    enter_idle(&mut mgr, &mut state, 0);

    let _ = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 1000 })
        .unwrap();
    assert_eq!(state.step_index(), 1);

    let _ = mgr
        .handle_event(
            &mut state,
            Event::UserActivity {
                kind: ActivityKind::Any,
                now_ms: 1500,
            },
        )
        .unwrap();

    assert_eq!(state.step_index(), 0);
    assert_eq!(state.step_base_ms(), 1500);
}

#[test]
fn notify_then_run_with_delay() {
    let mut s = step(PlanStepKind::Dpms, 5, "doit");
    s.notification = Some("warn".to_string());
    s.notify_seconds_before = Some(3);

    let mut mgr = Manager::new(cfg_with_plan_and_notify(vec![s], 2, true));
    let mut state = State::new(0);
    state.set_plan_source(PlanSource::Desktop);
    enter_idle(&mut mgr, &mut state, 0);

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 4999 })
        .unwrap();
    assert!(actions.is_empty());

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 5000 })
        .unwrap();
    assert_eq!(
        actions,
        vec![Action::Notify {
            message: "warn".to_string()
        }]
    );

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 7999 })
        .unwrap();
    assert!(actions.is_empty());

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 8000 })
        .unwrap();
    assert_eq!(
        actions,
        vec![Action::RunCommand {
            command: "doit".to_string()
        }]
    );
}

#[test]
fn late_tick_runs_notify_then_command_on_later_tick() {
    // With current semantics:
    // - Notify is emitted first (when we first observe we're past base_due).
    // - Run happens notify_seconds_before AFTER the notify emission time.
    //
    // debounce=1s, timeout=4s => base_due=5s
    // notify_seconds_before=2s
    // Late tick at 9000ms:
    // - Notify emitted at 9000ms
    // - Run due at 11000ms (9000 + 2000)

    let mut s = step(PlanStepKind::Startup, 4, "go");
    s.notification = Some("heads up".to_string());
    s.notify_seconds_before = Some(2);

    let mut mgr = Manager::new(cfg_with_plan_and_notify(vec![s], 1, true));
    let mut state = State::new(0);
    state.set_plan_source(PlanSource::Desktop);
    enter_idle(&mut mgr, &mut state, 0);

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 9000 })
        .unwrap();

    assert_eq!(
        actions,
        vec![Action::Notify {
            message: "heads up".to_string()
        }]
    );

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 10999 })
        .unwrap();
    assert!(actions.is_empty());

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 11000 })
        .unwrap();

    assert_eq!(
        actions,
        vec![Action::RunCommand {
            command: "go".to_string()
        }]
    );
}

#[test]
fn no_notification_text_ignores_notify_seconds_before() {
    let mut s = step(PlanStepKind::Dpms, 5, "doit");
    s.notification = None;
    s.notify_seconds_before = Some(999);

    let mut mgr = Manager::new(cfg_with_plan_and_notify(vec![s], 2, true));
    let mut state = State::new(0);
    state.set_plan_source(PlanSource::Desktop);
    enter_idle(&mut mgr, &mut state, 0);

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 4999 })
        .unwrap();
    assert!(actions.is_empty());

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 5000 })
        .unwrap();
    assert_eq!(
        actions,
        vec![Action::RunCommand {
            command: "doit".to_string()
        }]
    );
}

#[test]
fn compositor_idled_without_resumed_fires_resume_and_restarts_window() {
    // Regression test for the niri workaround: when CompositorIdled arrives
    // while debounce is already cleared (no CompositorResumed in between),
    // stasis must (a) emit resume commands for any previously-fired step and
    // (b) restart the idle window from the new Idled timestamp.

    let mut s = step(PlanStepKind::Dpms, 5, "dpms_off");
    s.resume_command = Some("dpms_on".to_string());

    let mut mgr = Manager::new(cfg_with_plan(vec![s]));
    let mut state = State::new(0);
    state.set_plan_source(PlanSource::Desktop);

    // First idle window: step fires at t=5000.
    enter_idle(&mut mgr, &mut state, 0);
    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 5000 })
        .unwrap();
    assert_eq!(
        actions,
        vec![Action::RunCommand {
            command: "dpms_off".to_string()
        }]
    );
    // debounce_pending is still false; no CompositorResumed arrives (niri scenario).

    // Second CompositorIdled without any CompositorResumed in between.
    let actions = mgr
        .handle_event(&mut state, Event::CompositorIdled { now_ms: 10_000 })
        .unwrap();

    // (a) Resume command must fire for the previously-fired Dpms step.
    assert!(
        actions.iter().any(|a| matches!(
            a,
            Action::RunResumeCommand { command } if command == "dpms_on"
        )),
        "expected RunResumeCommand(dpms_on) in {actions:?}"
    );

    // (b) Idle window restarts from the new Idled timestamp.
    assert_eq!(state.step_base_ms(), 10_000);
    assert_eq!(state.step_index(), 0);

    // Step must not fire before new_base + timeout.
    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 14_999 })
        .unwrap();
    assert!(actions.is_empty());

    // Step fires exactly at 10_000 + 5_000 = 15_000.
    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 15_000 })
        .unwrap();
    assert_eq!(
        actions,
        vec![Action::RunCommand {
            command: "dpms_off".to_string()
        }]
    );
}

#[test]
fn browser_inhibit_stays_active_until_explicit_inactive_edge() {
    let plan = vec![step(PlanStepKind::Dpms, 1, "dpms")];

    let mut mgr = Manager::new(cfg_with_plan(plan));
    let mut state = State::new(0);
    state.set_plan_source(PlanSource::Desktop);

    let actions = mgr
        .handle_event(&mut state, Event::BrowserActivity { now_ms: 0 })
        .unwrap();
    assert!(actions.is_empty());

    // While browser inhibit is active, long ticks must not advance/fires steps.
    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 60_000 })
        .unwrap();
    assert!(actions.is_empty());

    // Explicit inactive edge starts idle timing from this point.
    let actions = mgr
        .handle_event(&mut state, Event::BrowserInactive { now_ms: 60_000 })
        .unwrap();
    assert!(actions.is_empty());

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 60_999 })
        .unwrap();
    assert!(actions.is_empty());

    let actions = mgr
        .handle_event(&mut state, Event::Tick { now_ms: 61_000 })
        .unwrap();
    assert_eq!(
        actions,
        vec![Action::RunCommand {
            command: "dpms".to_string()
        }]
    );
}
